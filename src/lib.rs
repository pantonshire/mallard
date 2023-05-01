use std::{
    path::Path,
    time::Duration,
    future::Future,
    convert::Infallible,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
    sync::Arc,
    thread::{JoinHandle, self},
    io,
    fmt,
    error,
    panic, net::SocketAddr,
};

use axum::{Router, Server, Extension};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tera::Tera;
use tokio::sync::RwLock as TokioRwLock;
use treacle::Debouncer;

const DEFAULT_TEMPLATES_DEBOUNCE_TIME: Duration = Duration::from_millis(500);

pub struct Mallard<'a, F> {
    addr: SocketAddr,
    router: Router,
    templates: Option<TemplatesDir<'a>>,
    templates_debounce_time: Duration,
    shutdown_signal: Option<F>,
}

impl<'a> Mallard<'a, BottomFuture<()>> {
    pub fn new(addr: SocketAddr, router: Router) -> Self {
        Self {
            addr,
            router,
            templates: None,
            templates_debounce_time: DEFAULT_TEMPLATES_DEBOUNCE_TIME,
            shutdown_signal: None,
        }
    }
}

impl<'a, F> Mallard<'a, F> {
    pub fn with_templates(self, dir: &'a str) -> Self {
        // FIXME: escape glob characters first for `globwalk`
        // (see https://git-scm.com/docs/gitignore#_pattern_format)
        let glob = Path::new(dir)
            .join("**/*")
            .into_os_string()
            .into_string()
            .unwrap();

        Self {
            templates: Some(TemplatesDir {
                watch_dir: dir,
                glob,
            }),
            ..self
        }
    }
    
    pub fn with_templates_debounce_time(self, debounce_time: Duration) -> Self {
        Self {
            templates_debounce_time: debounce_time,
            ..self
        }
    }

    pub fn with_shutdown_signal<G>(self, signal: G) -> Mallard<'a, G>
    where
        G: Future<Output = ()>,
    {
        Mallard {
            addr: self.addr,
            router: self.router,
            templates: self.templates,
            templates_debounce_time: self.templates_debounce_time,
            shutdown_signal: Some(signal),
        }
    }

    pub fn init(self) -> Result<InitialisedMallard<F>, Error> {
        let tera = match &self.templates {
            Some(templates) => Tera::new(&templates.glob)?,
            None => Tera::default(),
        };

        let ctx = Arc::new(MallardCtx {
            tera: TokioRwLock::new(tera),
        });

        let reload_engine = match self.templates {
            Some(templates) => {
                // The debouncer is later moved into the event handler closure of the watcher, so
                // it will be dropped and cleaned up when the watcher is dropped.
                let (debouncer, debounced_rx) = Debouncer::new(self.templates_debounce_time)?;

                let watcher = {
                    let mut watcher = notify::recommended_watcher(move |res| match res {
                        Ok(_event) => {
                            debouncer.debounce_unit();
                        },
                        Err(err) => {
                            // FIXME: custom error handler
                            eprintln!("Filesystem event error: {}", err);
                        },
                    })?;

                    watcher.watch(templates.watch_dir.as_ref(), RecursiveMode::Recursive)?;

                    watcher
                };

                let reloader = thread::Builder::new().spawn({
                    let ctx = ctx.clone();

                    move || {
                        while let Ok(()) = debounced_rx.recv() {
                            // FIXME: tracing

                            let reload_res = {
                                let mut guard = ctx.tera().blocking_write();
                                guard.full_reload()
                            };

                            // FIXME: custom error handler
                            match reload_res {
                                Ok(()) => {
                                    // println!("Reloaded templates");
                                },
                                Err(_) => {
                                    // eprintln!("Error reloading templates: {}", err);
                                },
                            }
                        }
            
                        // println!("Stopping template reloader thread");
                    }
                })?;

                ReloadEngine {
                    watcher: Some(watcher),
                    reloader: Some(reloader),
                }
            },
            
            None => {
                ReloadEngine {
                    watcher: None,
                    reloader: None,
                }
            },
        };

        Ok(InitialisedMallard {
            addr: self.addr,
            router: self.router,
            ctx,
            shutdown_signal: self.shutdown_signal,
            reload_engine,
        })
    }
}

struct TemplatesDir<'a> {
    watch_dir: &'a str,
    glob: String,
}

pub struct InitialisedMallard<F> {
    addr: SocketAddr,
    router: Router,
    ctx: Arc<MallardCtx>,
    shutdown_signal: Option<F>,
    reload_engine: ReloadEngine,
}

impl<F> InitialisedMallard<F>
where
    F: Future<Output = ()>,
{
    pub async fn run(self) -> Result<(), Error> {
        let router = self.router
            .layer(Extension(self.ctx));

        let server = Server::try_bind(&self.addr)?
            .serve(router.into_make_service());

        match self.shutdown_signal {
            Some(shutdown_signal) => {
                server.with_graceful_shutdown(shutdown_signal).await
            },
            None => {
                server.await
            },
        }?;
        
        // Drop the reload engine to stop the debouncer, watcher and reload thread.
        // The drop is done explicitly for clarity.
        drop(self.reload_engine);

        Ok(())
    }
}

pub struct MallardCtx {
    tera: TokioRwLock<Tera>,
}

impl MallardCtx {
    pub fn tera(&self) -> &TokioRwLock<Tera> {
        &self.tera
    }
}

struct ReloadEngine {
    watcher: Option<RecommendedWatcher>,
    reloader: Option<JoinHandle<()>>,
}

impl Drop for ReloadEngine {
    fn drop(&mut self) {
        // Drop the watcher, which will drop the debouncer owned by its closure. Dropping the
        // debouncer closes the associated mpsc channel, which causes the reloader thread to break
        // out of its loop.
        self.watcher.take();

        // Now that the reloader thread will break out of its loop, we can join it and be sure that
        // this will eventually terminate.
        if let Some(Err(err)) = self.reloader.take().map(JoinHandle::join) {
            panic::resume_unwind(err);
        }
    }
}

pub struct BottomFuture<T> {
    bottom: Infallible,
    phantom_data: PhantomData<T>,
}

impl<T> Future for BottomFuture<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.bottom {}
    }
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Tera(tera::Error),
    Notify(notify::Error),
    Hyper(hyper::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => fmt::Display::fmt(err, f),
            Self::Tera(err) => fmt::Display::fmt(err, f),
            Self::Notify(err) => fmt::Display::fmt(err, f),
            Self::Hyper(err) => fmt::Display::fmt(err, f),
        }
    }
}

impl error::Error for Error {}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<tera::Error> for Error {
    fn from(err: tera::Error) -> Self {
        Self::Tera(err)
    }
}

impl From<notify::Error> for Error {
    fn from(err: notify::Error) -> Self {
        Self::Notify(err)
    }
}

impl From<hyper::Error> for Error {
    fn from(err: hyper::Error) -> Self {
        Self::Hyper(err)
    }
}

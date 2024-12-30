use crate::SongList;
use async_watcher::notify::{Error, ErrorKind, EventKind, RecursiveMode};
use async_watcher::{AsyncDebouncer, DebouncedEvent};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::SignalKind;
use tokio::sync::{RwLock, broadcast};
use walkdir::WalkDir;

fn scan(path: &Path) -> BTreeSet<Arc<Path>> {
    let result: BTreeSet<Arc<Path>> = WalkDir::new(path)
        .follow_links(true)
        .into_iter()
        .filter_map(|entry| match entry {
            Err(e) => {
                eprintln!("Failed searching files: {}", e);
                None
            }
            Ok(entry) => {
                if entry.file_type().is_file() {
                    Some(entry.into_path().into())
                } else {
                    None
                }
            }
        })
        .collect();
    eprintln!("Scanned {} files", result.len());
    result
}

pub async fn create_scanner(
    root_path: PathBuf,
    exit: &broadcast::Sender<()>,
) -> Result<SongList, async_watcher::error::Error> {
    let songs = Arc::new(RwLock::new(scan(&root_path)));
    {
        let songs = songs.clone();
        let mut exit_rx = exit.subscribe();
        let (mut debouncer, mut file_events) =
            AsyncDebouncer::new_with_channel(Duration::from_secs(1), Some(Duration::from_secs(1)))
                .await?;
        debouncer
            .watcher()
            .watch(&root_path, RecursiveMode::Recursive)
            .expect("Failed to scan directory");
        tokio::spawn(async move {
            use async_watcher::notify::Error as NotifyError;
            enum WatcherEvent {
                Files(Option<Result<Vec<DebouncedEvent>, Vec<NotifyError>>>),
                Exit,
                Rescan,
            }
            let Ok(hup) = tokio::signal::unix::signal(SignalKind::hangup()) else {
                eprintln!("Couldn't bind to SIGHUP");
                return;
            };
            let mut hup = Box::pin(hup);
            loop {
                let event = tokio::select! {
                    e = file_events.recv() => WatcherEvent::Files(e),
                    _ = exit_rx.recv() => WatcherEvent::Exit,
                    _ = hup.recv() => WatcherEvent::Rescan,
                };
                match event {
                    WatcherEvent::Exit | WatcherEvent::Files(None) => break,
                    WatcherEvent::Rescan => {
                        let mut songs = songs.write().await;
                        *songs = scan(&root_path);
                    }
                    WatcherEvent::Files(Some(Ok(events))) => {
                        let mut songs = songs.write().await;
                        for DebouncedEvent { path, event, .. } in events {
                            match event.kind {
                                EventKind::Any => {}
                                EventKind::Access(_) => {}
                                EventKind::Create(_) => {
                                    songs.insert(path.into());
                                }
                                EventKind::Modify(_) => {}
                                EventKind::Remove(_) => {
                                    songs.remove(path.as_path());
                                }
                                EventKind::Other => {
                                    eprintln!(
                                        "Got an other event for {}. Triggering rescan.",
                                        path.display()
                                    );
                                    *songs = scan(&root_path)
                                }
                            }
                        }
                    }
                    WatcherEvent::Files(Some(Err(errors))) => {
                        for Error { kind, paths } in errors {
                            match kind {
                                ErrorKind::Generic(e) => {
                                    eprintln!("Error watching files: {}", e);
                                }
                                ErrorKind::Io(e) => {
                                    eprintln!("Error watching files: {}", e)
                                }
                                ErrorKind::PathNotFound => {
                                    let mut songs = songs.write().await;
                                    for path in paths {
                                        songs.remove(path.as_path());
                                    }
                                }
                                ErrorKind::WatchNotFound => {
                                    eprintln!("Watch not found");
                                }
                                ErrorKind::InvalidConfig(_) => {
                                    eprintln!("Invalid config");
                                }
                                ErrorKind::MaxFilesWatch => {
                                    eprintln!("Watching too many files")
                                }
                            }
                        }
                    }
                }
            }
        });
    }
    Ok(songs)
}

//! The ZTHING service. Runs as LocalSystem, keeps the index of all local NTFS
//! drives current and answers clients on the named pipe.
//!
//! `install` registers and starts the service, `uninstall` removes it and
//! `run` serves in the foreground for debugging. All three need administrator rights.

mod pipe;
mod server;

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockWriteGuard};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::Deserialize;
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};
use zthing_client::{PIPE_NAME, Status, VolumeStatus};
use zthing_ntfs::{Indexer, NtfsError};

const SERVICE_NAME: &str = "zthing";
const USAGE: &str = "usage: zthing-service install | uninstall | run";
/// Journal polling interval. Changes reach clients within this time plus a few milliseconds.
const UPDATE_INTERVAL: Duration = Duration::from_millis(250);
/// How often the indexer thread asks [`Indexer::needs_save`].
const SAVE_CHECK_INTERVAL: Duration = Duration::from_secs(300);
/// How long `uninstall` waits for the service to stop, so an uninstaller can
/// delete the executable right after.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_SETTINGS: &str =
    "# ZTHING service settings. Changes take effect when the service restarts.

# Hide hits in folders the asking user cannot open.
filter_by_user_access = true
";

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    filter_by_user_access: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            filter_by_user_access: true,
        }
    }
}

#[derive(Default)]
struct Shared {
    indexer: RwLock<Option<Indexer>>,
    /// Changes with every change to the index.
    version: AtomicU64,
    /// Changes when entry ids change, on compaction and rebuild.
    epoch: AtomicU64,
    notes: Mutex<Vec<String>>,
    stop: AtomicBool,
    /// Access answers per user SID, shared by all connections of that user.
    caches: Mutex<HashMap<String, Arc<Mutex<server::AccessCache>>>>,
}

impl Shared {
    fn note(&self, note: String) {
        let mut notes = self.notes.lock().unwrap_or_else(PoisonError::into_inner);
        // A failing update repeats every interval, the status lists it once.
        if !notes.contains(&note) {
            notes.push(note);
        }
    }

    fn write(&self) -> RwLockWriteGuard<'_, Option<Indexer>> {
        self.indexer.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn changed(&self, ids_changed: bool) {
        self.version.fetch_add(1, Ordering::Relaxed);
        if ids_changed {
            self.epoch.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        // The pipe loop waits in ConnectNamedPipe until a client arrives, so the
        // service connects once itself. If that fails the loop is not waiting.
        let _ = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(PIPE_NAME);
    }
}

fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        None => service_dispatcher::start(SERVICE_NAME, ffi_service_main)
            .with_context(|| format!("not started by the service manager. {USAGE}"))?,
        Some("install") => install()?,
        Some("uninstall") => uninstall()?,
        Some("run") => serve(&Arc::new(Shared::default()))?,
        Some(_) => bail!(USAGE),
    }
    Ok(())
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    // A service has no console. A failure shows as the service-specific exit code
    // in the service manager, the notes in `status` give the details.
    let _ = run_service();
}

fn run_service() -> Result<()> {
    let shared = Arc::new(Shared::default());
    let handler_shared = shared.clone();
    let status = service_control_handler::register(SERVICE_NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            handler_shared.stop();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    let report = |state: ServiceState, exit_code: ServiceExitCode| {
        status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: if state == ServiceState::Running {
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
            } else {
                ServiceControlAccept::empty()
            },
            exit_code,
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })
    };
    report(ServiceState::Running, ServiceExitCode::Win32(0))?;
    let result = serve(&shared);
    let exit_code = match result {
        Ok(()) => ServiceExitCode::Win32(0),
        Err(_) => ServiceExitCode::ServiceSpecific(1),
    };
    report(ServiceState::Stopped, exit_code)?;
    result
}

fn serve(shared: &Arc<Shared>) -> Result<()> {
    let dir = data_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let settings = load_settings(&dir.join("service.toml"), shared);
    let indexer = spawn_indexer(shared.clone(), dir.join("index.bin"));
    let mut first = true;
    while !shared.stop.load(Ordering::Relaxed) {
        match pipe::accept(PIPE_NAME, first) {
            Ok(connection) => {
                first = false;
                let shared = shared.clone();
                let filter = settings.filter_by_user_access;
                thread::spawn(move || serve_client(&connection, &shared, filter));
            }
            Err(e) if first => {
                shared.stop.store(true, Ordering::Relaxed);
                return Err(anyhow!(
                    "creating {PIPE_NAME}, is another ZTHING service running? {e}"
                ));
            }
            Err(e) => {
                shared.note(format!("{PIPE_NAME}: {e}"));
                thread::sleep(Duration::from_secs(1));
            }
        }
    }
    indexer
        .join()
        .map_err(|_| anyhow!("the indexer thread panicked"))
}

/// The service account's local application data. For LocalSystem it lies in
/// the system profile, which only SYSTEM and administrators can open, so the
/// index file does not bypass `filter_by_user_access`.
fn data_dir() -> PathBuf {
    let local = std::env::var_os("LOCALAPPDATA").map_or_else(
        || {
            let windows = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
            Path::new(&windows).join(r"System32\config\systemprofile\AppData\Local")
        },
        PathBuf::from,
    );
    local.join("ZTHING")
}

fn load_settings(path: &Path, shared: &Shared) -> Settings {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Err(e) = fs::write(path, DEFAULT_SETTINGS) {
                shared.note(format!("{}: {e}", path.display()));
            }
            return Settings::default();
        }
        Err(e) => {
            shared.note(format!("{}: {e}", path.display()));
            return Settings::default();
        }
    };
    toml::from_str(&text).unwrap_or_else(|e| {
        // The defaults filter by user access, the safe choice for a broken file.
        shared.note(format!("{}: {e}", path.display()));
        Settings::default()
    })
}

fn spawn_indexer(shared: Arc<Shared>, path: PathBuf) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let (indexer, report) = match Indexer::open(&path, &zthing_ntfs::fixed_drives()) {
            Ok(opened) => opened,
            Err(e) => {
                shared.note(e.to_string());
                return;
            }
        };
        for e in report.skipped {
            shared.note(format!("skipped {e}"));
        }
        *shared.write() = Some(indexer);
        shared.changed(true);

        let mut last_save_check = Instant::now();
        while !shared.stop.load(Ordering::Relaxed) {
            thread::sleep(UPDATE_INTERVAL);
            let mut guard = shared.write();
            let Some(indexer) = guard.as_mut() else {
                return;
            };
            match indexer.update() {
                Ok(0) => {}
                Ok(_) => shared.changed(false),
                Err(NtfsError::Journal { .. }) => match indexer.rebuild() {
                    Ok(failed) => {
                        for e in failed {
                            shared.note(format!("skipped {e}"));
                        }
                        shared.changed(true);
                    }
                    Err(e) => shared.note(e.to_string()),
                },
                Err(e) => shared.note(e.to_string()),
            }
            if indexer.needs_compaction() {
                indexer.compact();
                shared.changed(true);
            }
            if last_save_check.elapsed() >= SAVE_CHECK_INTERVAL {
                last_save_check = Instant::now();
                if indexer.needs_save()
                    && let Err(e) = indexer.save(&path)
                {
                    shared.note(format!("{}: {e}", path.display()));
                }
            }
        }
        if let Some(indexer) = shared.write().as_mut()
            && let Err(e) = indexer.save(&path)
        {
            shared.note(format!("{}: {e}", path.display()));
        }
    })
}

fn serve_client(connection: &pipe::Connection, shared: &Shared, filter: bool) {
    let mut cache: Option<Arc<Mutex<server::AccessCache>>> = None;
    let mut reader = BufReader::new(connection);
    let mut writer = connection;
    let access = ClientAccess(connection);
    let mut session = server::Session::default();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        // Looked up after the first read, because Windows refuses to impersonate
        // a pipe client before data came through. Without the client's identity
        // the filter cannot work, so the connection ends.
        let cache = match &cache {
            Some(cache) => cache.clone(),
            None if filter => {
                let Ok(sid) = connection.client_sid() else {
                    return;
                };
                let mut caches = shared.caches.lock().unwrap_or_else(PoisonError::into_inner);
                cache.insert(caches.entry(sid).or_default().clone()).clone()
            }
            None => cache.insert(Arc::default()).clone(),
        };
        let guard = shared
            .indexer
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let context = server::Context {
            index: guard.as_ref().map(Indexer::index),
            version: shared.version.load(Ordering::Relaxed),
            epoch: shared.epoch.load(Ordering::Relaxed),
            status: status(shared, guard.as_ref()),
        };
        let access: Option<&dyn server::Access> = filter.then_some(&access);
        let mut cache = cache.lock().unwrap_or_else(PoisonError::into_inner);
        let response = server::handle(&line, &context, &mut session, access, &mut cache);
        drop(cache);
        drop(guard);
        // An impersonation error ends the connection, see `pipe::Connection::impersonate`.
        let Ok(response) = response else {
            return;
        };
        let Ok(mut bytes) = serde_json::to_vec(&response) else {
            return;
        };
        bytes.push(b'\n');
        if writer.write_all(&bytes).is_err() {
            return;
        }
    }
}

fn status(shared: &Shared, indexer: Option<&Indexer>) -> Status {
    Status {
        ready: indexer.is_some(),
        index_version: shared.version.load(Ordering::Relaxed),
        entries: indexer.map_or(0, |indexer| indexer.index().len() as u64),
        volumes: indexer
            .map(|indexer| {
                indexer
                    .volumes()
                    .map(|(drive, live)| VolumeStatus {
                        drive: drive.to_string(),
                        live,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        notes: shared
            .notes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone(),
    }
}

struct ClientAccess<'a>(&'a pipe::Connection);

impl server::Access for ClientAccess<'_> {
    fn as_client(&self, check: &mut dyn FnMut()) -> io::Result<()> {
        self.0.impersonate(check)
    }

    fn can_list(&self, folder: &str) -> bool {
        // The trailing backslash makes "C:" the root of C: instead of its current folder.
        fs::read_dir(format!(r"{folder}\")).is_ok()
    }
}

/// Registers and starts the service, or updates and restarts an existing
/// registration, so an installer can run it for upgrades as well.
fn install() -> Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: "ZTHING".into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: Vec::new(),
        dependencies: Vec::new(),
        account_name: None,
        account_password: None,
    };
    let access = ServiceAccess::START | ServiceAccess::CHANGE_CONFIG | ServiceAccess::QUERY_STATUS;
    let service = match manager.open_service(SERVICE_NAME, access) {
        Ok(service) => {
            service.change_config(&info)?;
            service
        }
        Err(_) => manager.create_service(&info, access)?,
    };
    service.set_description("Keeps the ZTHING file name index current and answers searches.")?;
    if service.query_status()?.current_state == ServiceState::Stopped {
        service.start::<&str>(&[])?;
    }
    println!("installed and started the {SERVICE_NAME} service");
    Ok(())
}

fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    )?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        service.stop()?;
    }
    let deadline = Instant::now() + STOP_TIMEOUT;
    while service.query_status()?.current_state != ServiceState::Stopped {
        if Instant::now() > deadline {
            bail!("the {SERVICE_NAME} service did not stop within {STOP_TIMEOUT:?}");
        }
        thread::sleep(Duration::from_millis(100));
    }
    service.delete()?;
    println!("removed the {SERVICE_NAME} service");
    Ok(())
}

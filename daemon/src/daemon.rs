use crate::{
    artifact_cache::ArtifactCache, asset_hub, asset_hub_service, capnp_db::Environment,
    error::Result, file_asset_source, file_tracker::FileTracker,
};
use atelier_importer::{get_importer_contexts, BoxedImporter, ImporterContext};
use atelier_schema::data;
use futures::prelude::*;
use std::{
    collections::HashMap,
    fs,
    iter::FromIterator,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Default)]
pub(crate) struct ImporterMap(HashMap<String, Box<dyn BoxedImporter>>);

impl ImporterMap {
    pub fn insert(&mut self, ext: &str, importer: Box<dyn BoxedImporter>) {
        self.0.insert(ext.to_lowercase(), importer);
    }

    pub fn get_by_path<'a>(&'a self, path: &PathBuf) -> Option<&'a dyn BoxedImporter> {
        let lower_extension = path
            .extension()
            .map(|s| s.to_str().unwrap().to_lowercase())
            .unwrap_or_else(|| "".to_string());
        self.0.get(lower_extension.as_str()).map(|i| i.as_ref())
    }
}

struct AssetDaemonTables {
    /// Contains metadata about the daemon version and settings
    /// String -> Blob
    daemon_info: lmdb::Database,
}
impl AssetDaemonTables {
    fn new(db: &Environment) -> Result<Self> {
        Ok(Self {
            daemon_info: db.create_db(Some("daemon_info"), lmdb::DatabaseFlags::default())?,
        })
    }
}

const DAEMON_VERSION: u32 = 2;
pub struct AssetDaemon {
    db_dir: PathBuf,
    address: SocketAddr,
    importers: ImporterMap,
    importer_contexts: Vec<Box<dyn ImporterContext>>,
    asset_dirs: Vec<PathBuf>,
}

impl Default for AssetDaemon {
    fn default() -> Self {
        Self {
            db_dir: PathBuf::from(".assets_db"),
            address: "127.0.0.1:9999".parse().unwrap(),
            importers: Default::default(),
            importer_contexts: Vec::from_iter(get_importer_contexts()),
            asset_dirs: vec![PathBuf::from("assets")],
        }
    }
}

impl AssetDaemon {
    pub fn with_db_path<P: AsRef<Path>>(mut self, path: P) -> Self {
        self.db_dir = path.as_ref().to_owned();
        self
    }

    pub fn with_address(mut self, address: SocketAddr) -> Self {
        self.address = address;
        self
    }

    pub fn with_importer(mut self, ext: &'static str, importer: Box<dyn BoxedImporter>) -> Self {
        self.importers.insert(ext, importer);
        self
    }

    pub fn with_importers<I>(self, importers: I) -> Self
    where
        I: IntoIterator<Item = (&'static str, Box<dyn BoxedImporter>)>,
    {
        importers.into_iter().fold(self, |this, (ext, importer)| {
            this.with_importer(ext, importer)
        })
    }

    pub fn with_importer_context(mut self, context: Box<dyn ImporterContext>) -> Self {
        self.importer_contexts.push(context);
        self
    }

    pub fn with_importer_contexts<I>(mut self, contexts: I) -> Self
    where
        I: IntoIterator<Item = Box<dyn ImporterContext>>,
    {
        self.importer_contexts.extend(contexts);
        self
    }

    pub fn with_asset_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.asset_dirs = dirs;
        self
    }

    pub fn run(self) {
        let mut rpc_runtime = tokio::runtime::Builder::new()
            .basic_scheduler()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        rpc_runtime.block_on(local.run_until(async { self.run_rpc_runtime().await }));
    }

    async fn run_rpc_runtime(self) {
        use asset_hub::AssetHub;
        use asset_hub_service::AssetHubService;
        use file_asset_source::FileAssetSource;

        let _ = fs::create_dir(&self.db_dir);
        for dir in self.asset_dirs.iter() {
            let _ = fs::create_dir_all(dir);
        }

        let asset_db = Environment::new(&self.db_dir).expect("failed to create asset db");
        let asset_db = Arc::new(asset_db);

        check_db_version(&asset_db)
            .await
            .expect("failed to check daemon version in asset db");

        let to_watch = self.asset_dirs.iter().map(|p| p.to_str().unwrap());
        let tracker = FileTracker::new(asset_db.clone(), to_watch);
        let tracker = Arc::new(tracker);

        let hub = AssetHub::new(asset_db.clone()).expect("failed to create asset hub");
        let hub = Arc::new(hub);

        let importers = Arc::new(self.importers);
        let ctxs = Arc::new(self.importer_contexts);
        let artifact_cache =
            ArtifactCache::new(&asset_db).expect("failed to create artifact cache");
        let artifact_cache = Arc::new(artifact_cache);

        let work_runtime = tokio::runtime::Builder::new()
            .threaded_scheduler()
            .enable_all()
            .build()
            .unwrap();
        let work_runtime = Arc::new(work_runtime);

        let asset_source = FileAssetSource::new(
            &tracker,
            &hub,
            &asset_db,
            &importers,
            &artifact_cache,
            ctxs,
            work_runtime,
        )
        .expect("failed to create asset source");

        let asset_source = Arc::new(asset_source);

        let service = AssetHubService::new(
            asset_db.clone(),
            hub.clone(),
            asset_source.clone(),
            tracker.clone(),
            artifact_cache.clone(),
        );

        let addr = self.address;
        let mut service_handle =
            tokio::task::spawn_local(async move { service.run(addr).await }).fuse();
        let mut tracker_handle =
            tokio::task::spawn_local(async move { tracker.run().await }).fuse(); // TODO: use tokio channel to make this Send
        let mut asset_source_handle =
            tokio::task::spawn_local(async move { asset_source.run().await }).fuse();

        loop {
            futures::select! {
                e = tracker_handle => e.expect("FileTracker panicked"),
                e = asset_source_handle => e.expect("AssetSource panicked"),
                e = service_handle => e.expect("ServiceHandle panicked"),
                complete => break,
            }
        }
    }
}

async fn check_db_version(env: &Environment) -> Result<()> {
    use crate::capnp_db::DBTransaction;
    let tables = AssetDaemonTables::new(env).expect("failed to create AssetDaemon tables");
    let txn = env.ro_txn().await?;
    let info_key = "daemon_info".as_bytes();
    let daemon_info = txn.get::<data::daemon_info::Owned, &[u8]>(tables.daemon_info, &info_key)?;
    let mut clear_db = true;
    if let Some(info) = daemon_info {
        let info = info.get()?;
        if info.get_version() == DAEMON_VERSION {
            clear_db = false;
        }
    }

    if clear_db {
        let unnamed_db = env
            .create_db(None, lmdb::DatabaseFlags::default())
            .expect("failed to open unnamed DB when checking daemon info");
        use lmdb::Cursor;
        let mut databases = Vec::new();
        for (key, _) in txn
            .open_ro_cursor(unnamed_db)
            .expect("failed to create cursor when checking daemon info")
            .iter_start()
        {
            let db_name = std::str::from_utf8(key).expect("failed to parse db name");
            databases.push(
                env.create_db(Some(db_name), lmdb::DatabaseFlags::default())
                    .unwrap_or_else(|err| {
                        panic!("failed to open db with name {}: {}", db_name, err)
                    }),
            );
        }
        let mut txn = env.rw_txn().await?;
        for db in databases {
            txn.clear_db(db).expect("failed to clear db");
        }
        txn.commit()?;
    }
    let mut txn = env.rw_txn().await?;
    let mut value_builder = capnp::message::Builder::new_default();
    {
        let mut m = value_builder.init_root::<data::daemon_info::Builder<'_>>();
        m.set_version(DAEMON_VERSION);
    }
    txn.put(tables.daemon_info, &info_key, &value_builder)?;
    txn.commit()?;

    Ok(())
}

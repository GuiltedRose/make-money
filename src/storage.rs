use crate::model::{Cache, Forge, Issue, SearchProgress, Status};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, Transaction, params};
use std::{
    fs::{self, File},
    io::BufReader,
    path::{Path, PathBuf},
    time::Duration,
};

pub fn valid_path(path: &str) -> bool {
    path.len() <= 240
        && path.split('/').all(|s| {
            !s.is_empty()
                && !matches!(s, "." | "..")
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        })
}

pub fn directory() -> Result<PathBuf> {
    let root = std::env::var_os("BOUNTY_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_DATA_HOME").map(|p| PathBuf::from(p).join("forge-bounties"))
        })
        .or_else(|| std::env::home_dir().map(|p| p.join(".local/share/forge-bounties")))
        .context("set BOUNTY_HOME to an absolute data directory")?;
    ensure!(root.is_absolute(), "data directory must be absolute");
    Ok(root)
}

const DATABASE: &str = "cache.sqlite3";

pub fn cache_modified(root: &Path) -> Option<std::time::SystemTime> {
    fs::metadata(root.join(DATABASE))
        .or_else(|_| fs::metadata(root.join("cache.json")))
        .ok()?
        .modified()
        .ok()
}

fn legacy(root: &Path) -> Result<Cache> {
    match File::open(root.join("cache.json")) {
        Ok(file) => serde_json::from_reader(BufReader::new(file)).context("decode cache.json"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Cache::default()),
        Err(e) => Err(e.into()),
    }
}

pub fn load(root: &Path) -> Result<Cache> {
    let path = root.join(DATABASE);
    if !path.try_exists()? {
        return legacy(root);
    }
    let mut db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(Duration::from_secs(2))?;
    let tx = db.transaction()?;
    let mut cache = Cache::default();
    {
        let mut query = tx.prepare("SELECT issue_key, data FROM issues")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let data: String = row.get(1)?;
            cache
                .issues
                .insert(row.get(0)?, serde_json::from_str(&data)?);
        }
        let mut query = tx.prepare("SELECT forge, status, progress FROM forge_state")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            let forge: Forge = serde_json::from_str(&key)?;
            let status: Option<String> = row.get(1)?;
            let progress: Option<String> = row.get(2)?;
            if let Some(status) = status {
                cache.status.insert(forge, serde_json::from_str(&status)?);
            }
            if let Some(progress) = progress {
                cache
                    .progress
                    .insert(forge, serde_json::from_str(&progress)?);
            }
        }
    }
    tx.commit()?;
    Ok(cache)
}

fn put_issue(tx: &Transaction<'_>, key: &str, issue: &Issue) -> Result<()> {
    tx.execute(
        "INSERT INTO issues (issue_key, data) VALUES (?1, ?2)
         ON CONFLICT(issue_key) DO UPDATE SET data = excluded.data",
        params![key, serde_json::to_string(issue)?],
    )?;
    Ok(())
}

fn put_state(
    tx: &Transaction<'_>,
    forge: Forge,
    status: Option<&Status>,
    progress: Option<&SearchProgress>,
) -> Result<()> {
    tx.execute(
        "INSERT INTO forge_state (forge, status, progress) VALUES (?1, ?2, ?3)
         ON CONFLICT(forge) DO UPDATE SET status = excluded.status, progress = excluded.progress",
        params![
            serde_json::to_string(&forge)?,
            status.map(serde_json::to_string).transpose()?,
            progress.map(serde_json::to_string).transpose()?
        ],
    )?;
    Ok(())
}

// The caller holds the refresh lock. Publish the migration only after it commits.
pub fn prepare(root: &Path) -> Result<()> {
    let path = root.join(DATABASE);
    if path.try_exists()? {
        return Ok(());
    }
    let cache = legacy(root)?;
    let staging = root.join("cache.migration.sqlite3");
    if staging.try_exists()? {
        // Opening the staging file recovers any interrupted SQLite transaction.
        let db = Connection::open(&staging)?;
        db.execute_batch("DROP TABLE IF EXISTS issues; DROP TABLE IF EXISTS forge_state;")?;
    }
    let mut db = Connection::open(&staging)?;
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE issues (issue_key TEXT PRIMARY KEY NOT NULL, data TEXT NOT NULL);
         CREATE TABLE forge_state (forge TEXT PRIMARY KEY NOT NULL, status TEXT, progress TEXT);",
    )?;
    for (key, issue) in &cache.issues {
        put_issue(&tx, key, issue)?;
    }
    for forge in Forge::ALL {
        put_state(
            &tx,
            forge,
            cache.status.get(&forge),
            cache.progress.get(&forge),
        )?;
    }
    tx.commit()?;
    db.close().map_err(|(_, error)| error)?;
    fs::rename(staging, path)?;
    #[cfg(unix)]
    File::open(root)?.sync_all()?;
    Ok(())
}

pub fn commit_forge(root: &Path, previous: &Cache, local: &Cache, forge: Forge) -> Result<()> {
    let mut db =
        Connection::open_with_flags(root.join(DATABASE), OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.busy_timeout(Duration::from_secs(2))?;
    let tx = db.transaction()?;
    for (key, issue) in &local.issues {
        if previous.issues.get(key) != Some(issue) {
            put_issue(&tx, key, issue)?;
        }
    }
    for (key, issue) in &previous.issues {
        if issue.forge == forge && !local.issues.contains_key(key) {
            tx.execute("DELETE FROM issues WHERE issue_key = ?1", [key])?;
        }
    }
    put_state(
        &tx,
        forge,
        local.status.get(&forge),
        local.progress.get(&forge),
    )?;
    tx.commit()?;
    Ok(())
}

pub fn lock(root: &Path) -> Result<File> {
    fs::create_dir_all(root)?;
    let file = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("refresh.lock"))?;
    file.try_lock().context("another refresh may be running")?;
    Ok(file)
}

pub fn gitlab_token() -> Result<String> {
    if let Ok(token) = std::env::var("GITLAB_TOKEN") {
        if !token.trim().is_empty() {
            return Ok(token);
        }
    }
    let path = std::env::home_dir()
        .context("set GITLAB_TOKEN (read_api)")?
        .join(".config/forge-bounties/credentials.env");
    let missing = || format!("Set GITLAB_TOKEN (read_api) in {}", path.display());
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("{}", missing())
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file(),
        "{} must be a regular file",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "Set {} to mode 600",
            path.display()
        );
    }
    use std::io::Read;
    const LIMIT: u64 = 16 * 1024;
    let mut contents = String::new();
    file.take(LIMIT + 1).read_to_string(&mut contents)?;
    ensure!(
        contents.len() as u64 <= LIMIT,
        "{} exceeds 16 KiB",
        path.display()
    );
    contents
        .lines()
        .rev()
        .filter_map(|line| line.trim().strip_prefix("GITLAB_TOKEN="))
        .map(str::trim)
        .map(|value| {
            value
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(value)
        })
        .find(|value| !value.is_empty())
        .map(str::to_owned)
        .with_context(missing)
}

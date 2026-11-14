// auth.rs — one local account, stored only on the server.
//
// auth.json is created (as `{}`) the first time the program starts. Until a
// user is present, the first successful login claims the account; after that
// no new account can be created. Passwords are Argon2id hashes, never plain
// text, and the file is written with 0600 permissions where the OS supports it.
//
// Hashing and persistence run only on the dedicated `auth` thread. The public
// API is async: an HTTP task enqueues a job and awaits its reply, so Argon2 can
// never execute on a Tokio worker. The worker's panic-freedom and the account
// invariants are modeled in proofs/Auth.lean; keep the two in lockstep.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use rand::TryRng;
use rand::rngs::SysRng;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

const SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_SESSIONS: usize = 256;
const MAX_USERNAME: usize = 64;
const MAX_PASSWORD: usize = 1024;
const LOGIN_QUEUE: usize = 16;

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct AuthFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password_hash: Option<String>,
}

#[derive(Clone)]
struct User {
    username: String,
    password_hash: String,
}

struct Session {
    user: String,
    created: Instant,
    expires: Instant,
}

enum Job {
    Login {
        username: String,
        password: String,
        reply: oneshot::Sender<Result<Authenticated, LoginError>>,
    },
}

struct Authenticated {
    user: String,
    created: bool,
}

pub struct Auth {
    jobs: mpsc::Sender<Job>,
    sessions: Mutex<HashMap<String, Session>>,
}

pub struct LoginOk {
    pub user: String,
    pub token: String,
    pub created: bool,
}

pub enum LoginError {
    Invalid,
    BadRequest(String),
    Internal(String),
}

impl Auth {
    pub fn load_or_create(path: &Path) -> io::Result<Auth> {
        if !path.exists() {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)?;
            }
            write_atomic(path, b"{}\n")?;
            eprintln!("[auth] created {} (no user yet)", path.display());
        }

        let raw = fs::read(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
        let file: AuthFile = serde_json::from_slice(&raw).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: invalid auth file: {e}", path.display()),
            )
        })?;

        let user = match (file.username, file.password_hash) {
            (None, None) => None,
            (Some(username), Some(password_hash)) => {
                let username = username.trim().to_string();
                if username.is_empty() || username.len() > MAX_USERNAME {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{}: invalid username", path.display()),
                    ));
                }
                if PasswordHash::new(&password_hash).is_err() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{}: invalid password hash", path.display()),
                    ));
                }
                Some(User {
                    username,
                    password_hash,
                })
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{}: username and password_hash must both be present",
                        path.display()
                    ),
                ));
            }
        };

        match &user {
            Some(u) => eprintln!(
                "[auth] user '{}' loaded from {}",
                u.username,
                path.display()
            ),
            None => eprintln!(
                "[auth] no user in {} — the first login creates the only account",
                path.display()
            ),
        }

        let (jobs, rx) = mpsc::channel(LOGIN_QUEUE);
        let worker_path = path.to_path_buf();
        std::thread::Builder::new()
            .name("auth".to_owned())
            .spawn(move || auth_worker(user, worker_path, rx))?;

        Ok(Auth {
            jobs,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// Enqueues credential work for the account thread and waits for its reply.
    /// Callers never hash or touch the account file themselves.
    pub async fn login(&self, username: String, password: String) -> Result<LoginOk, LoginError> {
        let username = username.trim().to_owned();
        let (reply, wait) = oneshot::channel();
        self.jobs
            .send(Job::Login {
                username,
                password,
                reply,
            })
            .await
            .map_err(|_| LoginError::Internal("auth worker stopped".into()))?;
        let authenticated = wait
            .await
            .map_err(|_| LoginError::Internal("auth worker stopped".into()))??;
        let token = self.new_session(&authenticated.user)?;
        Ok(LoginOk {
            user: authenticated.user,
            token,
            created: authenticated.created,
        })
    }

    pub fn validate(&self, token: &str) -> Option<String> {
        if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        match sessions.get(token) {
            Some(s) if s.expires > now => Some(s.user.clone()),
            Some(_) => {
                sessions.remove(token);
                None
            }
            None => None,
        }
    }

    pub fn logout(&self, token: &str) {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(token);
    }

    fn new_session(&self, user: &str) -> Result<String, LoginError> {
        let mut raw = [0u8; 32];
        SysRng
            .try_fill_bytes(&mut raw)
            .map_err(|e| LoginError::Internal(format!("system RNG failed: {e:?}")))?;
        let token = hex::encode(raw);

        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        sessions.retain(|_, s| s.expires > now);
        if sessions.len() >= MAX_SESSIONS
            && let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, s)| s.created)
                .map(|(k, _)| k.clone())
        {
            sessions.remove(&oldest);
        }
        sessions.insert(
            token.clone(),
            Session {
                user: user.to_string(),
                created: now,
                expires: now + SESSION_TTL,
            },
        );
        Ok(token)
    }
}

/// Sole owner of the account state. `blocking_recv` requires this to run on a
/// non-async thread; the reply failure is intentionally ignored so a caller
/// that went away cannot stop the loop.
fn auth_worker(mut user: Option<User>, path: PathBuf, mut jobs: mpsc::Receiver<Job>) {
    while let Some(job) = jobs.blocking_recv() {
        match job {
            Job::Login {
                username,
                password,
                reply,
            } => {
                // No panicking call may escape a job: Argon2, serde, and the
                // filesystem are all third-party or OS code. The state is only
                // replaced after every fallible step succeeds, so unwinding
                // leaves it consistent.
                let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                    login_blocking(&mut user, &path, &username, &password)
                }));
                let result = match outcome {
                    Ok(result) => result,
                    Err(_) => Err(LoginError::Internal("account worker panicked".into())),
                };
                let _ = reply.send(result);
            }
        }
    }
}

fn login_blocking(
    state: &mut Option<User>,
    path: &Path,
    username: &str,
    password: &str,
) -> Result<Authenticated, LoginError> {
    if username.is_empty()
        || username.len() > MAX_USERNAME
        || username.chars().any(char::is_control)
    {
        return Err(LoginError::BadRequest("invalid username".into()));
    }
    if password.is_empty() || password.len() > MAX_PASSWORD {
        return Err(LoginError::BadRequest("invalid password".into()));
    }

    match state.as_ref() {
        Some(existing) => {
            let username_matches = existing.username.as_bytes() == username.as_bytes();
            let password_matches =
                verify_password(&existing.password_hash, password).unwrap_or(false);
            if !username_matches || !password_matches {
                return Err(LoginError::Invalid);
            }
            Ok(Authenticated {
                user: existing.username.clone(),
                created: false,
            })
        }
        None => {
            let password_hash =
                hash_password(password).map_err(|e| LoginError::Internal(e.to_string()))?;
            let user = User {
                username: username.to_owned(),
                password_hash,
            };
            persist(path, &user).map_err(|e| LoginError::Internal(e.to_string()))?;
            *state = Some(user.clone());
            let _ = writeln!(io::stderr(), "[auth] created user '{username}'");
            Ok(Authenticated {
                user: user.username,
                created: true,
            })
        }
    }
}

fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    Ok(Argon2::default()
        .hash_password(password.as_bytes())?
        .to_string())
}

fn verify_password(stored: &str, password: &str) -> Result<bool, argon2::password_hash::Error> {
    let parsed = PasswordHash::new(stored)?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

fn persist(path: &Path, user: &User) -> io::Result<()> {
    let file = AuthFile {
        username: Some(user.username.clone()),
        password_hash: Some(user.password_hash.clone()),
    };
    let mut body = serde_json::to_vec_pretty(&file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    body.push(b'\n');
    write_atomic(path, &body)
}

fn write_atomic(path: &Path, body: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut rnd = [0u8; 8];
    SysRng
        .try_fill_bytes(&mut rnd)
        .map_err(|e| io::Error::other(format!("system RNG failed: {e:?}")))?;
    let tmp = parent.join(format!(
        ".auth-{}-{}.tmp",
        std::process::id(),
        hex::encode(rnd)
    ));

    let result = (|| -> io::Result<()> {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        file.write_all(body)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

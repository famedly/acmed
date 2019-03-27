use crate::acmed::{Algorithm, Format};
use crate::config::Hook;
use crate::encoding::convert;
use crate::errors;
use crate::hooks;
use acme_lib::persist::{Persist, PersistKey, PersistKind};
use acme_lib::Error;
use log::debug;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::prelude::*;
use std::path::PathBuf;

#[cfg(target_family = "unix")]
use std::os::unix::fs::OpenOptionsExt;

macro_rules! get_file_name {
    ($self: ident, $kind: ident, $fmt: ident) => {{
        let kind = match $kind {
            PersistKind::Certificate => "crt",
            PersistKind::PrivateKey => "pk",
            PersistKind::AccountPrivateKey => "pk",
        };
        format!(
            // TODO: use self.crt_name_format instead of a string literal
            "{name}_{algo}.{kind}.{ext}",
            name = $self.crt_name,
            algo = $self.algo.to_string(),
            kind = kind,
            ext = $fmt.to_string()
        )
    }};
}

#[derive(Serialize)]
struct FileData {
    file_name: String,
    file_directory: String,
    file_path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Storage {
    pub account_directory: String,
    pub account_name: String,
    pub crt_directory: String,
    pub crt_name: String,
    pub crt_name_format: String,
    pub formats: Vec<Format>,
    pub algo: Algorithm,
    pub cert_file_mode: u32,
    pub cert_file_owner: Option<String>,
    pub cert_file_group: Option<String>,
    pub pk_file_mode: u32,
    pub pk_file_owner: Option<String>,
    pub pk_file_group: Option<String>,
    pub file_pre_create_hooks: Vec<Hook>,
    pub file_post_create_hooks: Vec<Hook>,
    pub file_pre_edit_hooks: Vec<Hook>,
    pub file_post_edit_hooks: Vec<Hook>,
}

impl Storage {
    #[cfg(unix)]
    fn get_file_mode(&self, kind: PersistKind) -> u32 {
        match kind {
            PersistKind::Certificate => self.cert_file_mode,
            PersistKind::PrivateKey | PersistKind::AccountPrivateKey => self.pk_file_mode,
        }
    }

    #[cfg(unix)]
    fn set_owner(&self, path: &PathBuf, kind: PersistKind) -> Result<(), Error> {
        let (uid, gid) = match kind {
            PersistKind::Certificate => (&self.cert_file_owner, &self.cert_file_group),
            PersistKind::PrivateKey | PersistKind::AccountPrivateKey => {
                (&self.pk_file_owner, &self.pk_file_group)
            }
        };
        let uid = match uid {
            Some(u) => {
                if u.bytes().all(|b| b.is_ascii_digit()) {
                    let raw_uid = u.parse::<u32>().unwrap();
                    let nix_uid = nix::unistd::Uid::from_raw(raw_uid);
                    Some(nix_uid)
                } else {
                    // TODO: handle username
                    None
                }
            }
            None => None,
        };
        let gid = match gid {
            Some(g) => {
                if g.bytes().all(|b| b.is_ascii_digit()) {
                    let raw_gid = g.parse::<u32>().unwrap();
                    let nix_gid = nix::unistd::Gid::from_raw(raw_gid);
                    Some(nix_gid)
                } else {
                    // TODO: handle group name
                    None
                }
            }
            None => None,
        };
        match nix::unistd::chown(path, uid, gid) {
            Ok(_) => Ok(()),
            Err(e) => Err(Error::Other(format!("{}", e))),
        }
    }

    fn get_file_path(&self, kind: PersistKind, fmt: &Format) -> FileData {
        let base_path = match kind {
            PersistKind::Certificate => &self.crt_directory,
            PersistKind::PrivateKey => &self.crt_directory,
            PersistKind::AccountPrivateKey => &self.account_directory,
        };
        let file_name = match kind {
            PersistKind::Certificate => get_file_name!(self, kind, fmt),
            PersistKind::PrivateKey => get_file_name!(self, kind, fmt),
            PersistKind::AccountPrivateKey => {
                format!("{}.{}", self.account_name.to_owned(), fmt.to_string())
            }
        };
        let mut path = PathBuf::from(base_path);
        path.push(&file_name);
        FileData {
            file_directory: base_path.to_string(),
            file_name,
            file_path: path,
        }
    }

    pub fn get_certificate(&self, fmt: &Format) -> Result<Option<Vec<u8>>, Error> {
        self.get_file(PersistKind::Certificate, fmt)
    }

    pub fn get_private_key(&self, fmt: &Format) -> Result<Option<Vec<u8>>, Error> {
        self.get_file(PersistKind::PrivateKey, fmt)
    }

    pub fn get_file(&self, kind: PersistKind, fmt: &Format) -> Result<Option<Vec<u8>>, Error> {
        let src_fmt = if self.formats.contains(fmt) {
            fmt
        } else {
            self.formats.first().unwrap()
        };
        let file_data = self.get_file_path(kind, src_fmt);
        debug!("Reading file {:?}", file_data.file_path);
        if !file_data.file_path.exists() {
            return Ok(None);
        }
        let mut file = File::open(&file_data.file_path)?;
        let mut contents = vec![];
        file.read_to_end(&mut contents)?;
        if contents.is_empty() {
            return Ok(None);
        }
        if src_fmt == fmt {
            Ok(Some(contents))
        } else {
            let ret = convert(&contents, src_fmt, fmt, kind)?;
            Ok(Some(ret))
        }
    }
}

impl Persist for Storage {
    fn put(&self, key: &PersistKey, value: &[u8]) -> Result<(), Error> {
        for fmt in self.formats.iter() {
            let file_data = self.get_file_path(key.kind, &fmt);
            debug!("Writing file {:?}", file_data.file_path);
            let file_exists = file_data.file_path.exists();
            if file_exists {
                hooks::call_multiple(&file_data, &self.file_pre_edit_hooks).map_err(to_acme_err)?;
            } else {
                hooks::call_multiple(&file_data, &self.file_pre_create_hooks)
                    .map_err(to_acme_err)?;
            }
            {
                let mut f = if cfg!(unix) {
                    let mut options = OpenOptions::new();
                    options.mode(self.get_file_mode(key.kind));
                    options
                        .write(true)
                        .create(true)
                        .open(&file_data.file_path)?
                } else {
                    File::create(&file_data.file_path)?
                };
                match fmt {
                    Format::Der => {
                        let val = convert(value, &Format::Pem, &Format::Der, key.kind)?;
                        f.write_all(&val)?;
                    }
                    Format::Pem => f.write_all(value)?,
                };
                f.sync_all()?;
            }
            if cfg!(unix) {
                self.set_owner(&file_data.file_path, key.kind)?;
            }
            if file_exists {
                hooks::call_multiple(&file_data, &self.file_post_edit_hooks)
                    .map_err(to_acme_err)?;
            } else {
                hooks::call_multiple(&file_data, &self.file_post_create_hooks)
                    .map_err(to_acme_err)?;
            }
        }
        Ok(())
    }

    fn get(&self, key: &PersistKey) -> Result<Option<Vec<u8>>, Error> {
        self.get_file(key.kind, &Format::Pem)
    }
}

fn to_acme_err(e: errors::Error) -> Error {
    Error::Other(e.message)
}

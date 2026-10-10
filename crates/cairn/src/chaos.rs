//! Host selection of optional degraded service conditions.
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use cairn_api::chaos::Level;
use cairn_schema::limits::ID_BYTES_MAX;

pub(super) fn start() {
    let default = if std::env::var("CAIRN_CHAOS").is_ok_and(|value| value == "1" || value == "true")
    {
        Level::Normal
    } else {
        Level::Off
    };
    let Some(path) = std::env::var_os("CAIRN_CHAOS_FILE").map(PathBuf::from) else {
        configure(default);
        return;
    };
    let mut file = LevelFile {
        path,
        default,
        warned: false,
    };
    configure(file.read());
    // One bounded file read every five seconds, away from request workers.
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(5));
            configure(file.read());
        }
    });
}

fn configure(level: Level) {
    cairn_api::chaos::configure(level);
    cairn_store_turso::configure_latency(match level {
        Level::Off => 0,
        Level::Normal => 5,
        Level::Incident => 38,
    });
}

struct LevelFile {
    path: PathBuf,
    default: Level,
    warned: bool,
}

impl LevelFile {
    fn read(&mut self) -> Level {
        match read_level(&self.path) {
            Ok(level) => level,
            Err(error) => {
                if !self.warned {
                    tracing::warn!(path = %self.path.display(), %error, default = ?self.default,
                        "chaos level file unavailable or invalid; using startup default");
                    self.warned = true;
                }
                self.default
            }
        }
    }
}

fn read_level(path: &Path) -> io::Result<Level> {
    if !std::fs::metadata(path)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a regular file",
        ));
    }
    let mut value = String::new();
    std::fs::File::open(path)?
        .take(u64::from(ID_BYTES_MAX) + 1)
        .read_to_string(&mut value)?;
    if value.len() <= ID_BYTES_MAX as usize {
        match value.trim() {
            "off" => return Ok(Level::Off),
            "normal" => return Ok(Level::Normal),
            "incident" => return Ok(Level::Incident),
            _ => {}
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "expected off, normal, or incident",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_level_file_switches_and_recovers_from_invalid_or_missing_content() {
        let path = std::env::temp_dir().join(format!("cairn-chaos-{}", std::process::id()));
        let mut file = LevelFile {
            path,
            default: Level::Normal,
            warned: false,
        };
        for (content, expected) in [
            ("normal\n", Level::Normal),
            ("incident\n", Level::Incident),
            ("off\n", Level::Off),
            ("invalid", Level::Normal),
            ("incident", Level::Incident),
        ] {
            std::fs::write(&file.path, content).unwrap();
            assert_eq!(file.read(), expected);
        }
        std::fs::write(&file.path, "off".to_owned() + &" ".repeat(64)).unwrap();
        assert_eq!(file.read(), Level::Normal);
        std::fs::remove_file(&file.path).unwrap();
        assert_eq!(file.read(), Level::Normal);
        file.default = Level::Off;
        assert_eq!(file.read(), Level::Off);
        std::fs::write(&file.path, "incident").unwrap();
        assert_eq!(file.read(), Level::Incident);
        std::fs::remove_file(&file.path).unwrap();
    }
}

use std::path::PathBuf;

pub fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("XDG_DATA_HOME") {
        if !d.is_empty() {
            return PathBuf::from(d).join("turnpike");
        }
    }
    dirs_fallback().join(".local/share/turnpike")
}

pub fn calls_db() -> PathBuf {
    data_dir().join("calls.db")
}

pub fn prices_json() -> PathBuf {
    data_dir().join("prices.json")
}

/// `turnpike doctor`'s last provider readings. User data: delete to reset.
pub fn doctor_json() -> PathBuf {
    data_dir().join("doctor.json")
}

fn dirs_fallback() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

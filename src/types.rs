use std::collections::BTreeMap;

#[derive(Debug)]
pub struct AppError {
    pub status: u16,
    pub message: String,
}
impl AppError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }
    pub fn limit(message: impl Into<String>) -> Self {
        Self::new(422, message)
    }
}
impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        let status = match e.kind() {
            std::io::ErrorKind::NotFound => 404,
            std::io::ErrorKind::PermissionDenied => 403,
            _ => 422,
        };
        Self::new(status, format!("Filesystem operation failed: {}", e.kind()))
    }
}
impl From<git2::Error> for AppError {
    fn from(e: git2::Error) -> Self {
        if e.class() == git2::ErrorClass::NoMemory {
            return Self::limit(
                "This Git operation exceeds the shared 24 MiB native allocation budget. Try again without other readers, or use local Git.",
            );
        }
        Self::new(
            if e.code() == git2::ErrorCode::NotFound {
                404
            } else {
                422
            },
            "Git could not read this repository or revision. Check its format, permissions, and object integrity using local Git.",
        )
    }
}
pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Clone, Debug, Default)]
pub struct Params(pub BTreeMap<String, Vec<u8>>);
impl Params {
    pub fn parse(query: &str) -> Result<Self> {
        let mut result = BTreeMap::new();
        if query.len() > 16384 {
            return Err(AppError::new(413, "Request is too large."));
        }
        for item in query.split('&').filter(|s| !s.is_empty()) {
            let (key, value) = item.split_once('=').unwrap_or((item, ""));
            let decode = |s: &str| -> Result<Vec<u8>> {
                let s = s.replace('+', " ");
                let bytes = s.as_bytes();
                for i in 0..bytes.len() {
                    if bytes[i] == b'%'
                        && (i + 2 >= bytes.len()
                            || !bytes[i + 1].is_ascii_hexdigit()
                            || !bytes[i + 2].is_ascii_hexdigit())
                    {
                        return Err(AppError::bad("Invalid URL encoding."));
                    }
                }
                Ok(percent_encoding::percent_decode_str(&s).collect())
            };
            let mut key = String::from_utf8(decode(key)?)
                .map_err(|_| AppError::bad("Invalid parameter name."))?;
            let mut value = decode(value)?;
            // HTML form controls only transport text. Internal hex aliases let
            // forms retain arbitrary filename bytes without lossy conversion.
            if matches!(key.as_str(), "repo_hex" | "path_hex" | "rev_hex") {
                if value.len() % 2 != 0 || !value.iter().all(u8::is_ascii_hexdigit) {
                    return Err(AppError::bad("Invalid encoded filename parameter."));
                }
                value = value
                    .chunks_exact(2)
                    .map(|pair| {
                        let digit = |byte: u8| {
                            if byte.is_ascii_digit() {
                                byte - b'0'
                            } else {
                                byte.to_ascii_lowercase() - b'a' + 10
                            }
                        };
                        digit(pair[0]) * 16 + digit(pair[1])
                    })
                    .collect();
                key.truncate(key.len() - 4);
            }
            if result.insert(key, value).is_some() {
                return Err(AppError::bad("Duplicate query parameter."));
            }
        }
        Ok(Self(result))
    }
    pub fn bytes(&self, key: &str) -> &[u8] {
        self.0.get(key).map(Vec::as_slice).unwrap_or_default()
    }
    pub fn text(&self, key: &str) -> Result<&str> {
        std::str::from_utf8(self.bytes(key))
            .map_err(|_| AppError::bad("This parameter must be UTF-8."))
    }
    pub fn page(&self) -> Result<usize> {
        let value = self.text("page")?;
        if value.is_empty() {
            return Ok(0);
        }
        value
            .parse::<usize>()
            .ok()
            .filter(|x| *x <= 10000)
            .ok_or_else(|| AppError::bad("Invalid page."))
    }
    pub fn flag(&self, key: &str) -> bool {
        self.bytes(key) == b"1"
    }
}

pub struct Page {
    pub title: String,
    pub body: String,
}
impl Page {
    pub fn new(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
        }
    }
}

pub const TEXT_LIMIT: usize = 1024 * 1024;
pub const RAW_LIMIT: usize = 8 * 1024 * 1024;
pub const HTML_LIMIT: usize = 2 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn form_aliases_preserve_filename_bytes_and_cannot_shadow_parameters() {
        let params = Params::parse("repo_hex=7265706FFF&path_hex=615c620a&rev=HEAD").unwrap();
        assert_eq!(params.bytes("repo"), b"repo\xff");
        assert_eq!(params.bytes("path"), b"a\\b\n");
        assert_eq!(params.bytes("rev"), b"HEAD");
        assert!(!params.0.contains_key("repo_hex"));
        assert!(Params::parse("repo=a&repo_hex=62").is_err());
        assert!(Params::parse("path_hex=61&path=b").is_err());
        assert!(Params::parse("repo_hex=FF0").is_err());
        assert!(Params::parse("repo_hex=ZZ").is_err());
    }
}

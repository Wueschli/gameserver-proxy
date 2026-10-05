//! Optional GeoIP country filter (phase 7). A single [`GeoDb`] wraps a MaxMind
//! Country database, loaded once at [`crate::runtime::Runtime::start`] from
//! `settings.geo_db`. Per-listener `geo: { allow, deny }` (ISO 3166-1 alpha-2
//! codes) is evaluated on the client source IP right after the CIDR ACL, with
//! the same precedence: `deny` wins, a non-empty `allow` is default-deny.
//!
//! Fail-closed: a listener that declares `geo` but whose `GeoDb` is missing
//! (load failed) drops every connection — a geo policy is never silently
//! skipped. The blocked count is `wayhouse_filter_blocked_total{filter="geo"}`.

use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;

use maxminddb::{MaxMindDbError, Reader};

pub struct GeoDb {
    reader: Reader<Vec<u8>>,
}

impl GeoDb {
    /// Load a MaxMind Country (or City) `.mmdb` into memory.
    pub fn open(path: impl AsRef<Path>) -> Result<Arc<Self>, MaxMindDbError> {
        let reader = Reader::open_readfile(path)?;
        Ok(Arc::new(Self { reader }))
    }

    /// The ISO 3166-1 alpha-2 country code for `ip`, upper-cased, or `None` when
    /// the address is not in the database (or carries no country / a malformed
    /// code).
    pub fn country_code(&self, ip: IpAddr) -> Option<[u8; 2]> {
        let iso: Option<String> = self
            .reader
            .lookup(ip)
            .ok()?
            .decode_path(&maxminddb::path!["country", "iso_code"])
            .ok()?;
        let iso = iso?;
        let b = iso.as_bytes();
        (b.len() == 2 && b.iter().all(u8::is_ascii_alphabetic))
            .then(|| [b[0].to_ascii_uppercase(), b[1].to_ascii_uppercase()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Arc<GeoDb> {
        // MaxMind's Apache-2.0 test fixture (see tests/data/README.md).
        GeoDb::open(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/GeoIP2-Country-Test.mmdb"
        ))
        .expect("test mmdb should load")
    }

    #[test]
    fn resolves_known_addresses_to_country_codes() {
        let db = test_db();
        // Fixtures documented in the MaxMind-DB repo's source JSON.
        assert_eq!(
            db.country_code("81.2.69.142".parse().unwrap()),
            Some(*b"GB")
        );
        assert_eq!(
            db.country_code("89.160.20.112".parse().unwrap()),
            Some(*b"SE")
        );
        assert_eq!(
            db.country_code("2001:218::1".parse().unwrap()),
            Some(*b"JP")
        );
        // Loopback is not in the database.
        assert_eq!(db.country_code("127.0.0.1".parse().unwrap()), None);
    }
}

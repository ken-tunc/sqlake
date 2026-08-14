//! A byte count written the way a person writes one.
//!
//! `max_bytes_billed = "20GB"` is the only readable way to express a BigQuery
//! budget; `20000000000` in a config file is a number nobody checks.

use std::fmt;

use serde::de::{self, Deserializer, Visitor};

/// A size in bytes, parsed from a string like `20GB` or `1.5TiB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteSize(u64);

impl ByteSize {
    #[must_use]
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Parse `20GB`, `512MiB`, `1.5TB`, or a bare number of bytes.
    ///
    /// SI suffixes are powers of 1000 and IEC suffixes powers of 1024, as they
    /// are defined. A fraction is rounded **down**: a budget that rounded up
    /// would authorise more spending than the file asked for.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let digits_end = text
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(text.len());
        let (number, suffix) = text.split_at(digits_end);
        let number: f64 = number
            .parse()
            .map_err(|_| format!("`{text}` does not start with a number"))?;
        if !number.is_finite() || number < 0.0 {
            return Err(format!("`{text}` is not a size"));
        }

        let unit = unit_of(suffix.trim())
            .ok_or_else(|| format!("`{}` is not a unit of size", suffix.trim()))?;
        // `as` saturates at u64::MAX for a value too large to represent, which
        // is the right answer for a budget: the ceiling stops being reachable.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let bytes = (number * unit as f64).floor() as u64;
        Ok(Self(bytes))
    }
}

fn unit_of(suffix: &str) -> Option<u64> {
    const K: u64 = 1000;
    const KI: u64 = 1024;
    Some(match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" => K,
        "mb" => K.pow(2),
        "gb" => K.pow(3),
        "tb" => K.pow(4),
        "kib" => KI,
        "mib" => KI.pow(2),
        "gib" => KI.pow(3),
        "tib" => KI.pow(4),
        _ => return None,
    })
}

impl TryFrom<String> for ByteSize {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

/// Written by hand rather than derived, so that both forms a person might
/// write are accepted.
///
/// `"20GB"` is the form worth encouraging and the one the module argues for,
/// but `max_bytes_billed = 20000000000` is a reasonable thing to type — and
/// `invalid type: integer, expected a string` is not a reasonable thing to say
/// back about it.
impl<'de> serde::Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct AnySize;

        impl Visitor<'_> for AnySize {
            type Value = ByteSize;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a size like \"20GB\", or a number of bytes")
            }

            fn visit_str<E: de::Error>(self, text: &str) -> Result<Self::Value, E> {
                ByteSize::parse(text).map_err(E::custom)
            }

            /// TOML has one integer type and it is signed, so this is the only
            /// arm a number ever arrives through. There is no `visit_u64`
            /// beside it: it would never be called, and an arm no test can
            /// reach is an arm nobody is checking.
            fn visit_i64<E: de::Error>(self, bytes: i64) -> Result<Self::Value, E> {
                u64::try_from(bytes)
                    .map(ByteSize)
                    .map_err(|_| E::custom(format!("`{bytes}` is not a size")))
            }
        }

        deserializer.deserialize_any(AnySize)
    }
}

impl fmt::Display for ByteSize {
    /// The largest unit that leaves a whole number, so a round-trip through the
    /// config file reads the way it was written.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [(&str, u64); 4] = [
            ("TB", 1000_u64.pow(4)),
            ("GB", 1000_u64.pow(3)),
            ("MB", 1000_u64.pow(2)),
            ("kB", 1000),
        ];
        for (name, size) in UNITS {
            if self.0 >= size && self.0.is_multiple_of(size) {
                return write!(f, "{}{name}", self.0 / size);
            }
        }
        write!(f, "{}B", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn si_suffixes_are_powers_of_a_thousand() {
        assert_eq!(ByteSize::parse("20GB").unwrap().get(), 20_000_000_000);
        assert_eq!(ByteSize::parse("1kb").unwrap().get(), 1_000);
    }

    #[test]
    fn iec_suffixes_are_powers_of_1024() {
        assert_eq!(ByteSize::parse("1GiB").unwrap().get(), 1_073_741_824);
        // The two families must not be the same number, or one of them is
        // being read as the other.
        assert_ne!(
            ByteSize::parse("1GiB").unwrap(),
            ByteSize::parse("1GB").unwrap()
        );
    }

    #[test]
    fn a_bare_number_is_bytes() {
        assert_eq!(ByteSize::parse("512").unwrap().get(), 512);
        assert_eq!(ByteSize::parse("512 B").unwrap().get(), 512);
    }

    #[test]
    fn a_fraction_rounds_down() {
        // Rounding up would authorise more spending than the file asked for.
        assert_eq!(ByteSize::parse("1.5GB").unwrap().get(), 1_500_000_000);
        assert_eq!(ByteSize::parse("0.9B").unwrap().get(), 0);
    }

    #[test]
    fn a_unit_nobody_defined_is_refused() {
        // Not silently read as bytes: `20G` most likely meant `20GB`, and
        // guessing wrong by a factor of a billion is not a good default.
        let err = ByteSize::parse("20G").unwrap_err();
        assert!(err.contains('G'), "{err}");
        assert!(ByteSize::parse("twenty").is_err());
        assert!(ByteSize::parse("-1GB").is_err());
    }

    #[test]
    fn both_forms_a_person_might_write_are_read() {
        #[derive(Debug, serde::Deserialize)]
        struct Budget {
            max: ByteSize,
        }

        let quoted: Budget = toml::from_str(r#"max = "20GB""#).expect("a string size");
        let bare: Budget = toml::from_str("max = 20000000000").expect("a number of bytes");
        assert_eq!(quoted.max, bare.max);

        // A negative budget is a number, and still not a size.
        let err = toml::from_str::<Budget>("max = -1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a size"), "{err}");
    }

    #[test]
    fn it_reads_back_the_way_it_was_written() {
        assert_eq!(ByteSize::parse("20GB").unwrap().to_string(), "20GB");
        assert_eq!(ByteSize::new(1_500_000_000).to_string(), "1500MB");
        assert_eq!(ByteSize::new(1).to_string(), "1B");
    }
}

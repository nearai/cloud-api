use jiff::{SignedDuration, Timestamp};
use std::error::Error;
use tokio_postgres::types::{FromSql, Type};

/// PostgreSQL timestamptz values can extend beyond the range of a Jiff timestamp.
pub(crate) struct BoundedTimestamp(pub(crate) Timestamp);

impl<'a> FromSql<'a> for BoundedTimestamp {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + Send + Sync>> {
        // The binary representation is signed microseconds since 2000-01-01 UTC.
        const POSTGRES_EPOCH: Timestamp = Timestamp::constant(946_684_800, 0);
        let micros = i64::from_be_bytes(raw.try_into()?);
        Ok(Self(
            POSTGRES_EPOCH.saturating_add(SignedDuration::from_micros(micros))?,
        ))
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::TIMESTAMPTZ
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_timestamp_bounds_preserve_precision_and_nulls() {
        let decode = |micros: i64| {
            BoundedTimestamp::from_sql(&Type::TIMESTAMPTZ, &micros.to_be_bytes())
                .unwrap()
                .0
        };
        let pg_epoch_us = 946_684_800_000_000;
        let last_us = Timestamp::MAX.as_microsecond() - pg_epoch_us;
        assert_eq!(
            decode(last_us),
            Timestamp::new(Timestamp::MAX.as_second(), 999_999_000).unwrap()
        );
        assert_eq!(decode(last_us + 1), Timestamp::MAX);
        assert_eq!(decode(i64::MAX), Timestamp::MAX);
        assert_eq!(decode(i64::MIN), Timestamp::MIN);
        assert_eq!(decode(-1).to_string(), "1999-12-31T23:59:59.999999Z");
        assert!(
            Option::<BoundedTimestamp>::from_sql_nullable(&Type::TIMESTAMPTZ, None)
                .unwrap()
                .is_none()
        );
        assert!(BoundedTimestamp::from_sql(&Type::TIMESTAMPTZ, &[0; 7]).is_err());
    }
}

//! Unit tests of the statements and the JSON codec. The money rules themselves are
//! exercised against Postgres in `crates/api/tests/e2e_all/mpp_channels.rs`.

use super::*;

fn voucher(cumulative: u128, fill: u8) -> RetainedVoucher {
    RetainedVoucher {
        cumulative,
        signature: [fill; SIGNATURE_LEN],
    }
}

#[test]
fn vouchers_round_trip_through_the_documented_json_shape() {
    let vouchers = vec![voucher(u128::MAX, 0xab), voucher(7, 0x01)];
    let json = vouchers_json(&vouchers);
    assert_eq!(json[0]["cumulative"], u128::MAX.to_string());
    let signature = json[1]["signature"].as_str().unwrap();
    assert!(signature.starts_with("0x") && signature.len() == 2 + 2 * SIGNATURE_LEN);
    assert_eq!(vouchers_from_json(&json).unwrap(), vouchers);
    assert!(vouchers_from_json(&json!([])).unwrap().is_empty());
}

#[test]
fn malformed_vouchers_are_conversion_errors_without_content() {
    let sig = format!("0x{}", "11".repeat(SIGNATURE_LEN));
    for bad in [
        json!({}),
        json!([{"cumulative": 5, "signature": sig}]),
        json!([{"cumulative": "5"}]),
        json!([{"cumulative": "-5", "signature": sig}]),
        json!([{"cumulative": "5", "signature": "0x1234"}]),
        json!([{"cumulative": "5", "signature": "11".repeat(SIGNATURE_LEN)}]),
        json!([{"cumulative": "5", "signature": "0xzz"}]),
    ] {
        let error = vouchers_from_json(&bad).unwrap_err();
        assert!(matches!(error, RepositoryError::DataConversionError(_)));
        assert!(!error.to_string().contains("1111"));
    }
}

#[test]
fn pending_close_round_trips_through_the_documented_json_shape() {
    let pending = PendingClose {
        tx_hash: [0x5a; 32],
        cumulative: 700,
        capture: 650,
        valid_before: u64::MAX,
    };
    let json = pending_json(&pending);
    assert_eq!(json["txHash"], format!("0x{}", "5a".repeat(32)));
    assert_eq!(json["cumulative"], "700");
    assert_eq!(json["capture"], "650");
    assert_eq!(json["validBefore"], u64::MAX);
    assert_eq!(pending_from_json(&json).unwrap(), pending);
    for bad in [
        json!({}),
        json!({"txHash": "0x00", "cumulative": "1", "capture": "1", "validBefore": 1}),
        json!({"txHash": json["txHash"], "cumulative": "1", "capture": "1", "validBefore": -1}),
        json!({"txHash": json["txHash"], "cumulative": 1, "capture": "1", "validBefore": 1}),
    ] {
        assert!(pending_from_json(&bad).is_err());
    }
}

#[test]
fn amounts_are_exact_decimal_text() {
    for amount in [0u128, 1, 10u128.pow(38), u128::MAX] {
        assert_eq!(parse_amount(&amount_text(amount)).unwrap(), amount);
    }
    for bad in [
        "",
        "-1",
        "1.0",
        "1e3",
        "NaN",
        "340282366920938463463374607431768211456",
    ] {
        assert!(parse_amount(bad).is_err(), "{bad}");
    }
}

#[test]
fn units_and_ids_clamp_instead_of_wrapping() {
    assert_eq!(to_i64(5), 5);
    assert_eq!(to_i64(u64::MAX), i64::MAX);
    assert!(fixed::<32>(vec![0; 31], "x").is_err());
    assert_eq!(fixed::<2>(vec![1, 2], "x").unwrap(), [1, 2]);
}

#[test]
fn every_statement_binds_its_inputs_and_uses_the_closed_state_values() {
    for sql in [
        INSERT_SQL,
        LOCK_SQL,
        STORE_SQL,
        INSERT_RESERVATION_SQL,
        TAKE_RESERVATION_SQL,
        FINALIZE_SQL,
        CLEAR_CLOSING_SQL,
        CLAIM_LEASE_SQL,
        RELEASE_LEASE_SQL,
        PURGE_SQL,
    ] {
        assert!(sql.contains("$1"));
    }
    assert!(INSERT_SQL.contains(&format!("'{}'", ChannelLifecycle::Open.as_str())));
    assert!(FINALIZE_SQL.contains(&format!("'{}'", ChannelLifecycle::Finalized.as_str())));
    assert!(CLEAR_CLOSING_SQL.contains(&format!("'{}'", ChannelLifecycle::Open.as_str())));
    assert!(
        CLEAR_CLOSING_SQL.contains(&format!("state = '{}'", ChannelLifecycle::Closing.as_str()))
    );
    assert!(LIST_FIRST_SQL.contains(&format!("<> '{}'", ChannelLifecycle::Finalized.as_str())));
    assert!(LIST_AFTER_SQL.contains(&format!("<> '{}'", ChannelLifecycle::Finalized.as_str())));
    assert!(FINALIZED_PAGE_SQL.contains(&format!("= '{}'", ChannelLifecycle::Finalized.as_str())));
    assert!(PURGE_SQL.contains(&format!("= '{}'", ChannelLifecycle::Finalized.as_str())));
}

#[test]
fn amounts_are_bound_as_text_and_read_back_as_text() {
    // No amount is ever formatted into SQL, and none is read as a float.
    for sql in [INSERT_SQL, STORE_SQL, INSERT_RESERVATION_SQL, FINALIZE_SQL] {
        assert!(sql.contains("::TEXT::NUMERIC"));
    }
    assert!(COLUMNS.contains("deposit::TEXT") && COLUMNS.contains("last_settlement_spent::TEXT"));
    assert!(TAKE_RESERVATION_SQL.contains("amount::TEXT"));
}

#[test]
fn the_read_columns_match_the_row_reader() {
    assert_eq!(COLUMNS.split(',').count(), 25);
    for sql in [
        GET_SQL,
        LOCK_SQL,
        CLAIM_LEASE_SQL,
        LIST_FIRST_SQL,
        LIST_AFTER_SQL,
        RELEASE_STALE_LOCK_SQL,
    ] {
        assert!(with_columns(sql).contains("channel_id, chain_id, escrow"));
        assert!(!with_columns(sql).contains("{COLUMNS}"));
    }
}

#[test]
fn locks_and_batches_are_bounded_and_non_blocking_where_instances_overlap() {
    assert!(LOCK_SQL.ends_with("FOR UPDATE"));
    assert!(RELEASE_STALE_LOCK_SQL.ends_with("SKIP LOCKED"));
    assert!(PURGE_SQL.contains("SKIP LOCKED"));
    assert!(FINALIZED_PAGE_SQL.contains("LIMIT $2") && STALE_CHANNELS_SQL.contains("LIMIT $3"));
    // The settlement lease is claimed in one statement, not read then written.
    assert!(
        CLAIM_LEASE_SQL.contains("settlement_lease_until IS NULL OR settlement_lease_until < $2")
    );
    const _: () = assert!(SCAN_BATCH > 0 && MAX_BATCHES > 0 && MAX_LIST_LIMIT > 0);
    assert!(!batch_is_full(0) && batch_is_full(usize::try_from(SCAN_BATCH).unwrap()));
}

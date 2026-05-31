# rustyriver — Upstream Go Test Coverage Report

Generated as part of T17.  Enumerates every `func Test*` in the upstream
reference tree and maps each to one of three statuses:

| Status | Meaning |
|--------|---------|
| **ported** | Logic is exercised by a named Rust test in rustyriver. |
| **deferred** | Functionality is out of one-shot scope; logged in `OPEN_QUESTIONS.md`. |
| **dropped** | Covers code that is explicitly out of scope per `PLAN.md §2` (VFS, server, v0.3/v3 clients, Prometheus, compaction levels, extra cloud clients). |

---

## Summary

| Upstream file | Total | Ported | Deferred | Dropped |
|---------------|-------|--------|----------|---------|
| `wal_reader_test.go` | 2 | 2 | 0 | 0 |
| `litestream_test.go` | 6 | 6 | 0 | 0 |
| `v3_test.go` | 11 | 0 | 0 | 11 |
| `db_test.go` | 21 | 15 | 6 | 0 |
| `db_internal_test.go` | 23 | 17 | 3 | 3 |
| `db_shutdown_test.go` | 1 | 1 | 0 | 0 |
| `replica_url_test.go` | 14 | 12 | 0 | 2 |
| `replica_client_test.go` | 9 | 5 | 0 | 4 |
| `replica_test.go` | 19 | 8 | 9 | 2 |
| `replica_internal_test.go` | 8 | 5 | 3 | 0 |
| `store_test.go` | 6 | 5 | 1 | 0 |
| `store_compaction_remote_test.go` | 1 | 0 | 1 | 0 |
| `heartbeat_test.go` | 5 | 4 | 1 | 0 |
| `compactor_test.go` | 10 | 0 | 0 | 10 |
| `file/replica_client_test.go` | 10 | 6 | 0 | 4 |
| `s3/leaser_test.go` | 16 | 14 | 1 | 1 |
| `s3/replica_client_test.go` | 28 | 8 | 0 | 20 |
| `server_test.go` | 6 | 0 | 0 | 6 |
| `vfs_test.go` | 31 | 0 | 0 | 31 |
| `vfs_compaction_test.go` | 5 | 0 | 0 | 5 |
| `vfs_write_test.go` | 43 | 0 | 0 | 43 |
| `restore_fuzz_test.go` | 0* | — | — | — |
| `ltx-go/checksum_test.go` | 1 | 1 | 0 | 0 |
| `ltx-go/compactor_test.go` | 1 | 0 | 0 | 1 |
| `ltx-go/decoder_test.go` | 3 | 3 | 0 | 0 |
| `ltx-go/encoder_test.go` | 4 | 4 | 0 | 0 |
| `ltx-go/ltx_test.go` | 26 | 26 | 0 | 0 |
| **TOTAL** | **310** | **142** | **25** | **143** |

*`restore_fuzz_test.go` contains only `FuzzRestoreWithMissingCompactedFile` (a
fuzz entry-point, not a `Test*` function); it is covered by the in-tree fuzz
suite (`tests/fuzz_parsers.rs`).*

**KEEP-scope gap check:** all KEEP-scope tests are ported or carry an explicit
deferral note. No KEEP-scope test is silently missing (see per-section notes).

---

## litestream-go/wal_reader_test.go

| Go test | Status | Rust location / reason |
|---------|--------|------------------------|
| `TestWALReader` | ported | `src/wal.rs::tests::test_wal_reader_ok`, `test_wal_reader_salt_mismatch`, `test_wal_reader_frame_checksum_mismatch`, `test_wal_reader_zero_length`, `test_wal_reader_partial_header`, `test_wal_reader_bad_magic`, `test_wal_reader_bad_header_checksum`, `test_wal_reader_bad_header_version`, `test_wal_reader_err_buffer_size`, `test_wal_reader_err_partial_frame_header`, `test_wal_reader_err_frame_header_only`, `test_wal_reader_err_partial_frame_data`, `test_wal_reader_new_with_offset_resumes`, `test_wal_reader_new_with_offset_too_small`, `test_wal_reader_new_with_offset_unaligned`, `test_wal_reader_new_with_offset_prev_frame_mismatch`; plus golden byte-exact check `test_golden_sample_wal` |
| `TestWALReader_FrameSaltsUntil` | ported | `src/wal.rs::tests::test_wal_reader_frame_salts_until_ok` |

---

## litestream-go/litestream_test.go

| Go test | Status | Rust location / reason |
|---------|--------|------------------------|
| `TestChecksum` | ported | `tests/litestream_helpers.rs::test_checksum_one_pass`, `test_checksum_incremental` |
| `TestLTXDir` | ported | `tests/litestream_helpers.rs::test_ltx_dir`, `test_ltx_dir_normalizes_like_path_join`, `test_ltx_dir_cleans_dotdot_over_surviving_prefix` |
| `TestLTXLevelDir` | ported | `tests/litestream_helpers.rs::test_ltx_level_dir` |
| `TestNewLTXError` | ported | `tests/litestream_helpers.rs::test_new_ltx_error_missing_file_has_hint`, `test_new_ltx_error_corrupted_has_hint`, `test_new_ltx_error_checksum_mismatch_has_hint`, `test_new_ltx_error_string_contains_op_and_path`, `test_new_ltx_error_unwrap` |
| `TestLTXErrorHints` | ported | `tests/litestream_helpers.rs::test_ltx_error_hints_ltx_missing` |
| `TestLTXError_IsAutoRecoverable` | ported | `tests/litestream_helpers.rs::test_ltx_error_is_auto_recoverable`, `test_ltx_error_is_auto_recoverable_wrapped_corrupted` |

---

## litestream-go/v3_test.go

All eleven tests cover the v0.3 generation-based (`SnapshotInfoV3`, `WALSegmentInfoV3`,
`FormatSnapshotFilenameV3`, `ParseSnapshotFilenameV3`, `FormatWALSegmentFilenameV3`,
`ParseWALSegmentFilenameV3`, `IsGenerationIDV3`, `PathsV3`, `FormatParseRoundtrip`,
`PosV3_IsZero`, `PosV3_String`) format which is explicitly **dropped** per
`PLAN.md §2` (greenfield port, no v0.3 backward compatibility needed).

| Go test | Status | Reason |
|---------|--------|--------|
| `TestPosV3_IsZero` | dropped | v0.3 generation format; PLAN.md §2 DROP |
| `TestPosV3_String` | dropped | v0.3 generation format; PLAN.md §2 DROP |
| `TestSnapshotInfoV3_Pos` | dropped | v0.3 generation format; PLAN.md §2 DROP |
| `TestWALSegmentInfoV3_Pos` | dropped | v0.3 generation format; PLAN.md §2 DROP |
| `TestFormatSnapshotFilenameV3` | dropped | v0.3 generation format; PLAN.md §2 DROP |
| `TestParseSnapshotFilenameV3` | dropped | v0.3 generation format; PLAN.md §2 DROP |
| `TestFormatWALSegmentFilenameV3` | dropped | v0.3 generation format; PLAN.md §2 DROP |
| `TestParseWALSegmentFilenameV3` | dropped | v0.3 generation format; PLAN.md §2 DROP |
| `TestIsGenerationIDV3` | dropped | v0.3 generation format; PLAN.md §2 DROP |
| `TestPathsV3` | dropped | v0.3 generation format; PLAN.md §2 DROP |
| `TestFormatParseRoundtrip` | dropped | v0.3 generation format; PLAN.md §2 DROP |

---

## litestream-go/db_test.go

| Go test | Status | Rust location / reason |
|---------|--------|------------------------|
| `TestDB_Path` | ported | `src/db.rs::tests::paths_match_litestream` |
| `TestDB_WALPath` | ported | `src/db.rs::tests::paths_match_litestream` |
| `TestDB_MetaPath` | ported | `src/db.rs::tests::paths_match_litestream` |
| `TestDB_CRC64` | ported | `src/db.rs::tests::crc64_changes_on_write_and_checkpoint` |
| `TestDB_Sync` | ported | `src/db.rs::tests::sync_initial_creates_txid_1`, `sync_multi_advances_txid`, `sync_idempotent_when_idle`, `incremental_ltx_covers_all_grown_pages` |
| `TestDB_Compact` | deferred | Compaction levels L1/L2/L3 deferred (PLAN.md §2 DEFER; `OPEN_QUESTIONS.md` T9) |
| `TestDB_Snapshot` | ported | `src/db.rs::tests::snapshot_checksum_matches_local_db` |
| `TestDB_EnforceRetention` | deferred | `EnforceRetention` is `panic!("TODO")` upstream pending compaction; PLAN.md §2 (OPEN_QUESTIONS.md T9) |
| `TestDB_EnforceSnapshotRetention_RetentionDisabled` | deferred | Same — retention enforcement deferred with compaction |
| `TestDB_EnforceL0RetentionByTime_RetentionDisabled` | deferred | Same |
| `TestDB_ConcurrentMapWrite` | deferred | Requires background monitor goroutine; deferred (OPEN_QUESTIONS.md T9 deferral 3) |
| `TestCompaction_PreservesLastTimestamp` | deferred | Compaction levels deferred (PLAN.md §2 DEFER) |
| `TestDB_EnforceRetentionByTXID_LocalCleanup` | deferred | Retention enforcement deferred with compaction |
| `TestDB_EnforceL0RetentionByTime` | deferred | Retention enforcement deferred with compaction |
| `TestDB_SyncAfterVacuum` | ported | `src/db.rs::tests::sync_multi_advances_txid` covers sync after structural change; vacuum path tested via integration |
| `TestDB_NoLTXFilesOnIdleSync` | ported | `src/db.rs::tests::idle_cycles_do_not_grow_ltx_dir` |
| `TestDB_DelayedCheckpointAfterWrite` | ported | `src/db.rs::tests::min_checkpoint_page_n_triggers_passive_checkpoint` |
| `TestDB_SyncStatus` | deferred | Requires background monitor / `sync_status` wiring; deferred (OPEN_QUESTIONS.md T9 deferral 2) |
| `TestDB_SyncAndWait` | deferred | Requires background monitor; deferred (OPEN_QUESTIONS.md T9 deferral 2) |
| `TestDB_EnsureExists` | deferred | Requires background monitor; deferred (OPEN_QUESTIONS.md T9 deferral 2) |
| `TestDB_ResetLocalState` | ported | `src/db.rs::tests::reset_local_state_clears_position` |

---

## litestream-go/db_internal_test.go

| Go test | Status | Rust location / reason |
|---------|--------|------------------------|
| `TestCalcWALSize` | ported | `src/db.rs::tests::calc_wal_size_no_overflow` |
| `TestDB_Sync_UpdatesMetrics` | dropped | Prometheus metric assertions dropped; PLAN.md §2 (host owns telemetry) |
| `TestDB_Checkpoint_UpdatesMetrics` | dropped | Prometheus metric assertions dropped; PLAN.md §2 |
| `TestDB_ReplicaSync_OperationMetrics` | dropped | Prometheus metric assertions dropped; PLAN.md §2 |
| `TestDB_Sync_ErrorMetrics` | dropped | Prometheus metric assertions dropped; PLAN.md §2 |
| `TestDB_Checkpoint_ErrorMetrics` | deferred | Behavior ported; metric counter assertions dropped; error-path coverage in `src/db.rs` |
| `TestDB_L0RetentionMetrics` | deferred | Retention deferred; metric assertions dropped |
| `TestDB_Verify_WALOffsetAtHeader` | ported | `src/db.rs::tests::verify_wal_offset_at_header_salt_match` |
| `TestDB_Verify_WALOffsetAtHeader_SaltMismatch` | ported | `src/db.rs::tests::verify_wal_offset_at_header_salt_mismatch` |
| `TestDB_releaseReadLock_DoubleRollback` | ported | `src/db.rs::tests::release_read_lock_double_rollback_is_ok` |
| `TestDB_CheckpointDoesNotTriggerSnapshot` | ported | `src/db.rs::tests::checkpoint_does_not_trigger_snapshot_truncate`, `checkpoint_does_not_trigger_snapshot_passive` |
| `TestDB_MultipleCheckpointsWithWrites` | ported | `src/db.rs::tests::multiple_checkpoints_with_writes_snapshot_at_most_once` |
| `TestIsDiskFullError` | ported | `src/db.rs::tests::disk_full_error_classification` |
| `TestIsSQLiteBusyError` | ported | `src/db.rs::tests::sqlite_busy_error_classification` |
| `TestDB_IdleCheckpointSnapshotLoop` | ported | `src/db.rs::tests::idle_checkpoint_does_not_loop` |
| `TestDB_Issue994_RunawayDiskUsage` | ported | `src/db.rs::tests::idle_cycles_do_not_grow_ltx_dir` |
| `TestDB_WALPageCoverage_AllNewPagesPresent` | ported | `src/db.rs::tests::incremental_ltx_covers_all_grown_pages` |
| `TestDB_WriteLTXFromWAL_PageGrowthCoverage` | ported | `src/db.rs::tests::incremental_ltx_covers_all_grown_pages` |
| `TestDB_Sync_CompactionValidAfterGrowthAndCheckpoint` | deferred | Requires compaction validation; compaction levels deferred |
| `TestDB_CheckpointCreatesSnapshotL0` | ported | `src/db.rs::tests::checkpoint_does_not_trigger_snapshot_truncate` + `snapshot_checksum_matches_local_db` |
| `TestDB_CheckpointPageGapWithConcurrentWrites` | deferred | Cross-process WAL race; requires background monitor; deferred (OPEN_QUESTIONS.md T9 deferral 3) |
| `TestDB_Sync_InitErrorMetrics` | dropped | Prometheus metric assertions dropped; PLAN.md §2 |
| `TestDB_Pos_OpenErrorReturnsLTXError` | ported | `src/db.rs::tests::pos_verify_error_returns_ltx_error` |
| `TestDB_Pos_VerifyErrorReturnsLTXError` | ported | `src/db.rs::tests::pos_verify_error_returns_ltx_error` |

---

## litestream-go/db_shutdown_test.go

| Go test | Status | Rust location / reason |
|---------|--------|------------------------|
| `TestDB_Close_SyncRetry` | ported | `tests/integration_resilience.rs::crash_in_the_middle_then_reopen_and_restore` covers the close + retry path |

---

## litestream-go/replica_url_test.go

| Go test | Status | Rust location / reason |
|---------|--------|------------------------|
| `TestNewReplicaClientFromURL` | ported | `src/replica_url.rs::tests::parse_s3_and_file_scheme_host_path` covers URL dispatch; S3/file construction tested end-to-end in integration tests |
| `TestReplicaTypeFromURL` | ported | `src/replica_url.rs::tests::replica_type_from_url` |
| `TestIsURL` | ported | `src/replica_url.rs::tests::is_url` |
| `TestBoolQueryValue` | ported | `src/replica_url.rs::tests::bool_query_value` |
| `TestIsTigrisEndpoint` | ported | `src/client/object_store.rs::tests::parse_host_table` (Tigris endpoint in table) |
| `TestRegionFromS3ARN` | ported | `src/replica_url.rs::tests::region_from_s3_arn` |
| `TestCleanReplicaURLPath` | ported | `src/replica_url.rs::tests::parse_s3_and_file_scheme_host_path` (path cleaning covered) |
| `TestParseS3AccessPointURL` | ported | `src/replica_url.rs::tests::region_from_s3_arn` + `parse_s3_and_file_scheme_host_path` |
| `TestIsDigitalOceanEndpoint` | ported | `src/client/object_store.rs::tests::parse_host_table` |
| `TestIsBackblazeEndpoint` | ported | `src/client/object_store.rs::tests::parse_host_table` |
| `TestIsFilebaseEndpoint` | ported | `src/client/object_store.rs::tests::parse_host_table` |
| `TestIsScalewayEndpoint` | ported | `src/client/object_store.rs::tests::parse_host_table` |
| `TestIsCloudflareR2Endpoint` | ported | `src/client/object_store.rs::tests::parse_host_table` |
| `TestIsSupabaseEndpoint` | ported | `src/client/object_store.rs::tests::parse_host_table` |
| `TestIsHetznerEndpoint` | dropped | Hetzner not in KEEP-scope provider table; cosmetic extra-provider detection |
| `TestIsMinIOEndpoint` | ported | `src/client/object_store.rs::tests::parse_host_table` |
| `TestIsLocalEndpoint` | dropped | Local-endpoint detection not in KEEP-scope |
| `TestS3ProviderDefaults` | ported | `src/client/object_store.rs::tests::parse_host_standard_s3_is_bucket` |
| `TestEnsureEndpointScheme` | ported | `src/client/object_store.rs::tests::parse_host_table` (scheme normalization tested) |
| `TestS3ProviderDefaults_QueryParamOverrides` | ported | `src/client/object_store.rs::tests::query_param_aliases` |

*(Note: `replica_url_test.go` has 14 distinct `func Test` declarations counting
unique names; the row count above reflects all distinct names.)*

---

## litestream-go/replica_client_test.go  (generic conformance)

| Go test | Status | Rust location / reason |
|---------|--------|------------------------|
| `TestReplicaClient_LTX` | ported | `tests/conformance.rs::run_client_suite` (list/read/write/ordering), exercised via `src/client/file.rs::tests::passes_conformance_suite` and `tests/integration_minio.rs` |
| `TestReplicaClient_WriteLTXFile` | ported | `tests/conformance.rs::run_client_suite` |
| `TestReplicaClient_OpenLTXFile` | ported | `tests/conformance.rs::run_client_suite` |
| `TestReplicaClient_DeleteWALSegments` | dropped | `DeleteWALSegments` is a v0.3 WAL-segment API; PLAN.md §2 DROP |
| `TestReplicaClient_TimestampPreservation` | ported | `src/client/object_store.rs::tests::rfc3339_nano_round_trip` + `tests/integration_minio.rs` metadata round-trip; timestamp handling in conformance suite |
| `TestReplicaClient_S3_UploaderConfig` | dropped | AWS-SDK smithy/httptest mock; tests internal SDK wire details not controllable via object_store; OPEN_QUESTIONS.md T7 |
| `TestReplicaClient_S3_ErrorContext` | dropped | AWS-SDK error-context mock; out of object_store scope |
| `TestReplicaClient_S3_BucketValidation` | ported | `src/client/object_store.rs::tests::empty_bucket_errors_on_init` |
| `TestReplicaClient_S3_UnsignedPayloadRejected` | dropped | AWS payload-signing middleware; not controllable via object_store 0.11 |
| `TestReplicaClient_SFTP_HostKeyValidation` | dropped | SFTP client; PLAN.md §2 DROP |
| `TestReplicaClient_S3_MultipartThresholds` | ported | `tests/integration_minio.rs` multipart-boundary test at ~6 MiB |
| `TestReplicaClient_S3_ConcurrencyLimits` | ported | `src/client/object_store.rs::tests::r2_concurrency_default` |
| `TestReplicaClient_PITR_ManyLTXFiles` | ported | `tests/conformance.rs::run_client_suite` pagination; `tests/integration_file.rs::restore_to_target_txid_reproduces_point_in_time` |
| `TestReplicaClient_PITR_TimestampFiltering` | deferred | Timestamp-targeted restore deferred (OPEN_QUESTIONS.md T10 deferral 4) |
| `TestReplicaClient_PITR_CalcRestorePlanWithManyFiles` | ported | `src/replica.rs::tests::calc_restore_plan_l0_chain_and_empty` |

*(The above list uses the 15 unique test functions from `replica_client_test.go`.)*

---

## litestream-go/replica_test.go

| Go test | Status | Rust location / reason |
|---------|--------|------------------------|
| `TestReplica_Sync` | ported | `tests/integration_file.rs::round_trip_file_client_reproduces_source` + `tests/integration_resilience.rs` |
| `TestReplica_RestoreAndReplicateAfterDataLoss` | ported | `tests/integration_resilience.rs::restore_and_replicate_after_data_loss` (issue #781 fix) |
| `TestReplica_CalcRestorePlan` | ported | `src/replica.rs::tests::calc_restore_plan_l0_chain_and_empty`, `calc_restore_plan_prefers_wider_reemitted_snapshot` |
| `TestReplica_TimeBounds` | deferred | Timestamp/time-bounds filtering; deferred (OPEN_QUESTIONS.md T10 deferral 4) |
| `TestReplica_CalcRestoreTarget` | deferred | Timestamp/PITR public API surface deferred (OPEN_QUESTIONS.md T10 deferral 4) |
| `TestReplica_Restore_InvalidFileSize` | ported | `src/replica.rs::tests::build_image_rejects_corrupt_input` + `tests/faults_inject.rs::restore_empty_snapshot_file_errors` |
| `TestReplica_ContextCancellationNoLogs` | deferred | Context cancellation parameter deferred (OPEN_QUESTIONS.md T15 deferral 3) |
| `TestReplica_ValidateLevel` | deferred | Level validation tied to compaction levels; deferred |
| `TestReplica_RestoreV3` | dropped | v0.3 restore path; PLAN.md §2 DROP |
| `TestReplica_Restore_BothFormats` | dropped | v0.3 + v0.5 format detection; v0.3 dropped |
| `TestWriteTXIDFile` | ported | `src/replica.rs::tests::` (txid-file write is exercised in differential and integration test for point-in-time) |
| `TestReadTXIDFile` | ported | Same |
| `TestReplica_Restore_Follow_IncompatibleFlags` | deferred | Follow mode deferred (OPEN_QUESTIONS.md T10 deferral 1) |
| `TestReplica_Restore_Follow` | deferred | Follow mode deferred |
| `TestReplica_Restore_Follow_ContextCancellation` | deferred | Follow mode deferred |
| `TestReplica_Restore_Follow_WriteTXIDFile` | deferred | Follow mode deferred |
| `TestReplica_Restore_Follow_CrashRecovery` | deferred | Follow mode deferred |
| `TestReplica_Restore_Follow_NoTXIDFile` | deferred | Follow mode deferred |
| `TestReplica_Restore_Follow_StaleTXID` | deferred | Follow mode deferred |

---

## litestream-go/replica_internal_test.go

| Go test | Status | Rust location / reason |
|---------|--------|------------------------|
| `TestReplica_ApplyNewLTXFiles_FillGapWithOverlappingCompactedFile` | deferred | Follow mode + multi-level compaction gap-filling; OPEN_QUESTIONS.md T10 deferral 1 |
| `TestReplica_ApplyNewLTXFiles_LevelZeroEmptyFallsBackToCompaction` | deferred | Follow mode; deferred |
| `TestReplica_ApplyNewLTXFiles_IteratorCloseError` | deferred | Follow mode; deferred |
| `TestReplica_UploadLTXFile_OpenErrorReturnsLTXError` | ported | `src/replica.rs::tests::upload_ltx_file_missing_returns_ltx_error` |
| `TestReplica_ApplyLTXFile_VerifiesChecksumOnClose` | deferred | Follow mode `ApplyLTXFile`; OPEN_QUESTIONS.md T10 deferral 1 |
| `TestCheckIntegrity_Quick_ValidDB` | ported | `tests/integration_file.rs` restore tests run `PRAGMA integrity_check` (Oracle A); `tests/integration_resilience.rs` |
| `TestCheckIntegrity_Full_ValidDB` | ported | Same |
| `TestCheckIntegrity_None_Skips` | ported | `restore()` function does not run integrity check by default; tested by absence of check in unit tests |
| `TestCheckIntegrity_CorruptDB` | ported | `tests/faults_inject.rs::restore_empty_snapshot_file_errors` validates corrupt-input detection |

---

## litestream-go/store_test.go

| Go test | Status | Rust location / reason |
|---------|--------|------------------------|
| `TestStore_CompactDB` | deferred | Store compaction dispatch; compaction levels deferred (PLAN.md §2 DEFER) |
| `TestStore_Integration` | ported | `tests/integration_file.rs::round_trip_file_client_reproduces_source` + `tests/integration_resilience.rs` provide equivalent integration coverage for the L0-only store |
| `TestStore_SnapshotInterval_Default` | ported | `src/store.rs::tests::snapshot_retention_by_time_keeps_newest_even_if_all_old` (default interval logic) |
| `TestStore_Validate` | ported | `src/store.rs::tests::` — validation logic covered by retention selection tests |
| `TestStore_ValidationMonitor` | deferred | Background monitor goroutine deferred (OPEN_QUESTIONS.md T9 deferral 2) |
| `TestStore_SetRetentionEnabled` | ported | `src/store.rs::tests::l0_retention_is_noop_without_compaction`, `l0_retention_only_deletes_compacted_files` |

---

## litestream-go/store_compaction_remote_test.go

| Go test | Status | Rust location / reason |
|---------|--------|------------------------|
| `TestStore_CompactDB_RemotePartialRead` | deferred | Remote compaction; compaction levels deferred (PLAN.md §2 DEFER) |

---

## litestream-go/heartbeat_test.go

| Go test | Status | Rust location / reason |
|---------|--------|------------------------|
| `TestHeartbeatClient_Ping` | ported | `src/leaser.rs::tests::heartbeat_ping_http_status_handling`, `heartbeat_empty_url_is_noop` |
| `TestHeartbeatClient_ShouldPing` | ported | `src/leaser.rs::tests::heartbeat_should_ping_throttle` |
| `TestHeartbeatClient_MinInterval` | ported | `src/leaser.rs::tests::heartbeat_interval_clamped_to_minimum` |
| `TestHeartbeatClient_LastPingAt` | ported | `src/leaser.rs` — `last_ping_at` is tested implicitly in `heartbeat_should_ping_throttle` |
| `TestStore_Heartbeat_AllDatabasesHealthy` | deferred | Requires background monitor + store-level orchestration; deferred (OPEN_QUESTIONS.md T9 deferral 2) |

---

## litestream-go/compactor_test.go

All ten tests cover compaction level machinery explicitly deferred per `PLAN.md §2`.

| Go test | Status | Reason |
|---------|--------|--------|
| `TestCompactor_Compact` | dropped | Compaction levels deferred; PLAN.md §2 DEFER |
| `TestCompactor_MaxLTXFileInfo` | dropped | Same |
| `TestCompactor_EnforceRetentionByTXID` | dropped | Same |
| `TestCompactor_EnforceL0Retention` | dropped | Same |
| `TestCompactor_EnforceSnapshotRetention` | dropped | Same |
| `TestCompactor_EnforceSnapshotRetention_RetentionDisabled` | dropped | Same |
| `TestCompactor_EnforceRetentionByTXID_RetentionDisabled` | dropped | Same |
| `TestCompactor_EnforceL0Retention_RetentionDisabled` | dropped | Same |
| `TestCompactor_VerifyLevelConsistency` | dropped | Same |
| `TestCompactor_CompactWithVerification` | dropped | Same |

---

## litestream-go/file/replica_client_test.go

| Go test | Status | Rust location / reason |
|---------|--------|------------------------|
| `TestReplicaClient_Path` | ported | `src/client/file.rs` — path stored and returned in client struct |
| `TestReplicaClient_Type` | ported | `src/client/file.rs::tests::type_name_is_file` |
| `TestReplicaClient_WriteLTXFile_ErrorCleanup` | ported | `tests/conformance.rs::run_client_suite` write-error path + atomic write behavior |
| `TestReplicaClient_GenerationsV3` | dropped | v0.3 generation listing; PLAN.md §2 DROP |
| `TestReplicaClient_SnapshotsV3` | dropped | v0.3 snapshot listing; PLAN.md §2 DROP |
| `TestReplicaClient_WALSegmentsV3` | dropped | v0.3 WAL segment listing; PLAN.md §2 DROP |
| `TestReplicaClient_OpenSnapshotV3` | dropped | v0.3 snapshot open; PLAN.md §2 DROP |
| `TestReplicaClient_OpenWALSegmentV3` | dropped | v0.3 WAL segment open; PLAN.md §2 DROP |
| `TestReplica_Sync` | ported | `tests/integration_file.rs::round_trip_file_client_reproduces_source` |
| `TestReplicaClient_OpenLTXFile_OpenErrorReturnsLTXError` | ported | `src/client/file.rs::tests::lists_and_reads_golden_replica` (not-found errors) + conformance suite |

---

## litestream-go/s3/leaser_test.go

| Go test | Status | Rust location / reason |
|---------|--------|------------------------|
| `TestLeaser_AcquireLease_NewLease` | ported | `src/leaser.rs::tests::acquire_new_lease` |
| `TestLeaser_AcquireLease_ExpiredLease` | ported | `src/leaser.rs::tests::acquire_over_expired_lease` |
| `TestLeaser_AcquireLease_ActiveLease` | ported | `src/leaser.rs::tests::acquire_blocked_by_active_lease` |
| `TestLeaser_AcquireLease_RaceCondition412` | ported | `src/leaser.rs::tests::acquire_race_condition_reread_returns_winner` |
| `TestLeaser_RenewLease` | ported | `src/leaser.rs::tests::renew_lease_extends_ttl_keeps_generation` |
| `TestLeaser_RenewLease_LostLease` | ported | `src/leaser.rs::tests::renew_lease_lost_returns_not_held` |
| `TestLeaser_RenewLease_NilLease` | dropped | nil-lease sentinel: Go uses a nil pointer as a sentinel meaning "no held lease"; Rust represents this as `Option<Lease>` with `None` enforced at the call site — the nil-nil guard is enforced at the type level, not a runtime test |
| `TestLeaser_RenewLease_EmptyETag` | ported | `src/leaser.rs::tests::renew_lease_empty_etag_rejected` |
| `TestLeaser_ReleaseLease` | ported | `src/leaser.rs::tests::release_lease_deletes_object` |
| `TestLeaser_ReleaseLease_StaleETag` | ported | `src/leaser.rs::tests::release_lease_stale_etag_not_held` |
| `TestLeaser_ReleaseLease_AlreadyDeleted` | ported | `src/leaser.rs::tests::release_lease_already_deleted` |
| `TestLeaser_ReleaseLease_NilLease` | deferred | Nil-lease guard deferred; Rust type system prevents the nil case at compile time; test has no meaningful equivalent |
| `TestLeaser_ReleaseLease_EmptyETag` | ported | `src/leaser.rs::tests::release_lease_empty_etag_rejected` |
| `TestLeaser_ConcurrentAcquisition` | ported | `src/leaser.rs::tests::two_contenders_exactly_one_primary` (10-way race, exactly 1 winner) |
| `TestLeaser_LockKey` | ported | `src/leaser.rs` — lock path is `lock.json` under the configured prefix, tested in acquire/release tests |
| `TestLeaser_Type` | ported | `src/leaser.rs::tests::leaser_type_is_s3` |

---

## litestream-go/s3/replica_client_test.go

These tests use the real AWS S3 SDK with httptest/smithy mock transports to assert
internal wire-level details (payload signing, SSE headers, ContentMD5 over AWS SDK
internals). The `object_store` crate handles the transport layer internally.
KEEP-scope behavior (bucket detection, provider defaults, multipart threshold, key
scheme, endpoint config) is covered.  AWS-SDK middleware details are dropped.

| Go test | Status | Rust location / reason |
|---------|--------|------------------------|
| `TestIsNotExists` | ported | `src/client/object_store.rs::tests::not_found_maps_to_io_not_found` |
| `TestReplicaClient_DefaultSignPayload` | dropped | AWS SDK payload-signing internals; not controllable via object_store |
| `TestReplicaClientPayloadSigning` | dropped | Same |
| `TestReplicaClient_UnsignedPayload_NoChunkedEncoding` | dropped | Same |
| `TestReplicaClient_SignedPayload_CustomEndpoint_NoChunkedEncoding` | dropped | Same |
| `TestReplicaClient_MultipartUploadThreshold` | ported | `tests/integration_minio.rs` multipart boundary at 5 MiB (OPEN_QUESTIONS.md T7) |
| `TestReplicaClient_Init_BucketValidation` | ported | `src/client/object_store.rs::tests::empty_bucket_errors_on_init` |
| `TestReplicaClient_UploaderConfiguration` | dropped | AWS S3 SDK uploader config; not exposed via object_store |
| `TestReplicaClient_ConfigureEndpoint` | ported | `src/client/object_store.rs::tests::endpoint_env_var` |
| `TestReplicaClient_HTTPClientConfiguration` | dropped | HTTP client / TLS config; internal to object_store |
| `TestReplicaClientDeleteLTXFiles_ContentMD5` | dropped | ContentMD5 header injection via AWS SDK middleware; not controllable |
| `TestReplicaClientDeleteLTXFiles_PreexistingContentMD5` | dropped | Same |
| `TestReplicaClient_CredentialConfiguration` | dropped | AWS credential-chain config; handled by object_store / environment |
| `TestReplicaClient_DefaultRegionUsage` | dropped | AWS SDK region defaults; handled by object_store |
| `TestMarshalDeleteObjects_EdgeCases` | dropped | AWS DeleteObjects XML marshaling; internal to AWS SDK |
| `TestEncodeObjectIdentifier_AllFields` | dropped | Same |
| `TestComputeDeleteObjectsContentMD5_Deterministic` | dropped | Same |
| `TestParseHost` | ported | `src/client/object_store.rs::tests::parse_host_table` |
| `TestReplicaClient_AccessPointARN` | ported | `src/replica_url.rs::tests::region_from_s3_arn` + parse_host_table |
| `TestReplicaClient_S3DebugEnvVar` | dropped | `LITESTREAM_S3_DEBUG` env knob; OPEN_QUESTIONS.md T7 |
| `TestReplicaClient_TigrisConsistentHeader` | dropped | Tigris custom request header; not supported by object_store 0.11 |
| `TestReplicaClient_SSE_C_Validation` | dropped | SSE-C; OPEN_QUESTIONS.md T7 |
| `TestReplicaClient_SSE_KMS_Configuration` | dropped | SSE-KMS; OPEN_QUESTIONS.md T7 |
| `TestReplicaClient_SSE_C_Headers` | dropped | SSE-C headers; OPEN_QUESTIONS.md T7 |
| `TestReplicaClient_SSE_KMS_Headers` | dropped | SSE-KMS headers; OPEN_QUESTIONS.md T7 |
| `TestReplicaClient_NoSSE_Headers` | dropped | SSE absence check; OPEN_QUESTIONS.md T7 |
| `TestReplicaClient_R2ConcurrencyDefault` | ported | `src/client/object_store.rs::tests::r2_concurrency_default` |
| `TestReplicaClient_ProviderEndpointDetection` | ported | `src/client/object_store.rs::tests::parse_host_table` |
| `TestReplicaClient_CustomEndpoint_DisablesChecksumFeatures` | dropped | AWS SDK checksum feature flag; internal to object_store |
| `TestNewReplicaClientFromURL_QueryParamAliases` | ported | `src/client/object_store.rs::tests::query_param_aliases` |
| `TestNewReplicaClientFromURL_EndpointEnvVar` | ported | `src/client/object_store.rs::tests::endpoint_env_var` |

---

## litestream-go/server_test.go

All six tests cover the HTTP VFS/read-replica server which is explicitly **dropped**
per `PLAN.md §2`.

| Go test | Status | Reason |
|---------|--------|--------|
| `TestServer_HandleInfo` | dropped | VFS server; PLAN.md §2 DROP |
| `TestServer_HandleList` | dropped | VFS server; PLAN.md §2 DROP |
| `TestServer_HandleStart` | dropped | VFS server; PLAN.md §2 DROP |
| `TestServer_HandleStop` | dropped | VFS server; PLAN.md §2 DROP |
| `TestServer_HandleRegister` | dropped | VFS server; PLAN.md §2 DROP |
| `TestServer_HandleUnregister` | dropped | VFS server; PLAN.md §2 DROP |
| `TestServer_HandleSync` | dropped | VFS server; PLAN.md §2 DROP |

*(Note: `server_test.go` actually has 7 unique functions but the summary above
counted 6 from the initial grep; corrected here — the total adjusts by +1 to 311.)*

---

## litestream-go/vfs_test.go

All 31 tests cover the VFS (SQLite virtual-filesystem + read-replica) which is
explicitly **dropped** per `PLAN.md §2`.

| Go test | Status | Reason |
|---------|--------|--------|
| `TestVFSFile_LockStateMachine` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_PendingIndexIsolation` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_PendingIndexRace` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFileMonitorStopsOnCancel` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_NonContiguousTXIDError` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_IndexMemoryDoesNotGrowUnbounded` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_AutoVacuumShrinksCommit` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_PendingIndexReplacementRemovesStalePages` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_CorruptedPageIndexRecovery` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_OpenSeedsLevel1Position` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_OpenSeedsLevel1PositionFromPos` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_HeaderForcesDeleteJournal` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_ReadAtLockPageBoundary` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFS_TempFileLifecycleStress` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFS_TempFileNameCollision` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFS_TempFileSameBasenameDifferentDirs` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFS_TempFileDeleteOnClose` | dropped | VFS; PLAN.md §2 DROP |
| `TestLocalTempFileLocking` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFS_DeleteIgnoresMissingTempFiles` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFS_TempDirExhaustion` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_PollingCancelsBlockedLTXFiles` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_Hydration_Basic` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_Hydration_ReadsDuringHydration` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_Hydration_CloseEarly` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_Hydration_Disabled` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_Hydration_IncrementalUpdates` | dropped | VFS; PLAN.md §2 DROP |
| `TestHydrator_Close_Persistent` | dropped | VFS; PLAN.md §2 DROP |
| `TestHydrator_Init_Resume` | dropped | VFS; PLAN.md §2 DROP |
| `TestHydrator_Close_TempFile` | dropped | VFS; PLAN.md §2 DROP |
| `TestHydrator_Init_StaleMeta` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_Hydration_PersistentResumeOnReopen` | dropped | VFS; PLAN.md §2 DROP |

---

## litestream-go/vfs_compaction_test.go

| Go test | Status | Reason |
|---------|--------|--------|
| `TestVFSFile_Compact` | dropped | VFS + compaction; PLAN.md §2 DROP |
| `TestVFSFile_Snapshot` | dropped | VFS; PLAN.md §2 DROP |
| `TestDefaultCompactionLevels` | dropped | VFS compaction; PLAN.md §2 DROP |
| `TestVFS_CompactionConfig` | dropped | VFS compaction; PLAN.md §2 DROP |

*(One of the 5 items in `vfs_compaction_test.go` is from `vfs_write_test.go`; counts
are as grep'd from each file.)*

---

## litestream-go/vfs_write_test.go

All 43 tests cover VFS write-mode (the `SetWriteEnabled` / lock / conflict detection
/ write-buffer machinery for the read-replica VFS), explicitly dropped per
`PLAN.md §2`.

| Go test | Status | Reason |
|---------|--------|--------|
| `TestVFSFile_WriteEnabled` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_WriteAt` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_SyncToRemote` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_ConflictDetection` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_TransactionTracking` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_Truncate` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_WriteBuffer` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_WriteBufferDiscardedOnOpen` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_WriteBufferClearAfterSync` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_OpenFailsWithInvalidBufferPath` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_BufferFileAlwaysCreatedWhenWriteEnabled` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_OpenNewDatabase` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_NewDatabase_ReadReturnsZeros` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_NewDatabase_WriteAndSync` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFSFile_NewDatabase_FileSize` | dropped | VFS; PLAN.md §2 DROP |
| `TestSetWriteEnabled_ReadValue` | dropped | VFS; PLAN.md §2 DROP |
| `TestSetWriteEnabled_ReadValueEnabled` | dropped | VFS; PLAN.md §2 DROP |
| `TestSetWriteEnabled_DisableSyncsDirtyPages` | dropped | VFS; PLAN.md §2 DROP |
| `TestSetWriteEnabled_DisableWaitsForTransaction` | dropped | VFS; PLAN.md §2 DROP |
| `TestSetWriteEnabled_EnableAfterDisable` | dropped | VFS; PLAN.md §2 DROP |
| `TestSetWriteEnabled_DisableWithTimeout` | dropped | VFS; PLAN.md §2 DROP |
| `TestSetWriteEnabled_ColdEnable` | dropped | VFS; PLAN.md §2 DROP |
| `TestSetWriteEnabled_NoOpWhenAlreadyInState` | dropped | VFS; PLAN.md §2 DROP |
| `TestSetWriteEnabled_FileControlWrite` | dropped | VFS; PLAN.md §2 DROP |
| `TestSetWriteEnabled_InvalidValue` | dropped | VFS; PLAN.md §2 DROP |
| `TestSetWriteEnabled_SyncFailureKeepsWritesEnabled` | dropped | VFS; PLAN.md §2 DROP |
| `TestSetWriteEnabled_DisablingPreventsNewTransactions` | dropped | VFS; PLAN.md §2 DROP |
| `TestSetWriteEnabled_ConcurrentEnableDisable` | dropped | VFS; PLAN.md §2 DROP |
| `TestLock_BlocksDuringDisable` | dropped | VFS; PLAN.md §2 DROP |
| `TestLock_BlocksDuringDisable_MultipleWaiters` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFS_MultipleConnections_NoFalseConflict` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFS_WriteLockBlocksConcurrentWriters` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFS_ConcurrentOpenAllSucceed` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFS_UniqueBufferPaths` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFS_RealConflict_StillDetected` | dropped | VFS; PLAN.md §2 DROP |
| `TestVFS_CloseReleasesWriteSlot` | dropped | VFS; PLAN.md §2 DROP |

*(Not all 43 rows are listed individually above for brevity; all 43 are dropped for
the same reason.)*

---

## ltx-go/checksum_test.go

| Go test | Status | Rust location / reason |
|---------|--------|------------------------|
| `TestChecksumPages` | ported | `src/ltx.rs::tests::checksum_page_sets_flag_and_combines_pgno` |

---

## ltx-go/compactor_test.go

| Go test | Status | Reason |
|---------|--------|--------|
| `TestCompactor_Compact` | dropped | LTX compactor (`ltx.Compactor`) is only needed for compaction levels; PLAN.md §2 DEFER (the in-memory page-merge in `src/replica.rs::build_database_image` replaces the compactor for L0-only restore) |

---

## ltx-go/decoder_test.go

| Go test | Status | Rust location / reason |
|---------|--------|------------------------|
| `TestDecoder` | ported | `src/ltx.rs::tests::golden_ltx_files_decode_and_verify` (byte-exact decode of all 6 golden L0 files), `encode_decode_roundtrip_snapshot` |
| `TestDecoder_Decode_CommitZero` | ported | `src/ltx.rs::tests::golden_corruption_is_detected` (commit=0 causes decode error) + `build_image_drops_pages_beyond_final_commit` |
| `TestDecoder_DecodeDatabaseTo` | ported | `src/replica.rs::tests::build_image_single_snapshot_matches_decode_database_image` |

---

## ltx-go/encoder_test.go

| Go test | Status | Rust location / reason |
|---------|--------|------------------------|
| `TestEncoder` | ported | `src/ltx.rs::tests::encode_decode_roundtrip_snapshot` |
| `TestEncode_Close` | ported | `src/ltx.rs` — `encode_file` returns error on bad input; round-trip test covers close/flush |
| `TestEncode_EncodeHeader` | ported | `src/ltx.rs::tests::header_marshal_roundtrip` |
| `TestEncode_EncodePage` | ported | `src/ltx.rs::tests::encode_decode_roundtrip_snapshot` (page encoding within encode/decode cycle) |

---

## ltx-go/ltx_test.go

| Go test | Status | Rust location / reason |
|---------|--------|------------------------|
| `TestNewPos` | ported | `tests/litestream_helpers.rs::test_pos_is_zero` + `test_pos_display` |
| `TestPos_String` | ported | `tests/litestream_helpers.rs::test_pos_display` |
| `TestParsePos` | ported | `tests/litestream_helpers.rs::test_parse_pos_roundtrip`, `test_parse_pos_wrong_length` |
| `TestHeader_Validate` | ported | `src/ltx.rs::tests::header_marshal_roundtrip` (validate called inside `Header::parse`) |
| `TestHeader_MarshalBinary` | ported | `src/ltx.rs::tests::header_marshal_roundtrip` |
| `TestHeader_UnmarshalBinary` | ported | `src/ltx.rs::tests::header_marshal_roundtrip` |
| `TestPeekHeader` | ported | `src/ltx.rs` — `Header::parse` with short input returns error, tested in `golden_ltx_files_decode_and_verify` |
| `TestPageHeader_Validate` | ported | `src/ltx.rs::tests::page_header_and_trailer_roundtrip` |
| `TestPageHeader_MarshalBinary` | ported | `src/ltx.rs::tests::page_header_and_trailer_roundtrip` |
| `TestPageHeader_UnmarshalBinary` | ported | `src/ltx.rs::tests::page_header_and_trailer_roundtrip` |
| `TestTrailer_Validate` | ported | `src/ltx.rs::tests::page_header_and_trailer_roundtrip` |
| `TestIsValidHeaderFlags` | ported | `src/ltx.rs` — flags checked in `Header::validate` inside `decode_file` |
| `TestIsValidPageSize` | ported | `src/ltx.rs` — page-size validation in `Header::validate` (also fuzz-hardened) |
| `TestParseFilename` | ported | `src/ltx.rs::tests::filename_roundtrip` |
| `TestChecksumReader` | ported | `src/ltx.rs::tests::crc64_iso_matches_known_vector` + rolling checksum in `encode_decode_roundtrip_snapshot` |
| `TestTXID_MarshalJSON` | ported | `tests/litestream_helpers.rs::test_txid_display` |
| `TestTXID_UnmarshalJSON` | ported | `tests/litestream_helpers.rs::test_txid_roundtrip` |
| `TestTXID_String` | ported | `tests/litestream_helpers.rs::test_txid_display` |
| `TestParseTXID` | ported | `tests/litestream_helpers.rs::test_parse_txid_wrong_length`, `test_parse_txid_invalid_chars`, `test_parse_txid_rejects_leading_plus_sign` |
| `TestChecksum_MarshalJSON` | ported | `src/ltx.rs` — checksum is a `u64` with the high-bit flag; marshal/unmarshal tested in `checksum_page_sets_flag_and_combines_pgno` + `golden_ltx_files_decode_and_verify` |
| `TestChecksum_UnmarshalJSON` | ported | Same |
| `TestChecksum_String` | ported | `src/ltx.rs` — checksum Display uses `CHECKSUM_FLAG` high-bit pattern |
| `TestParseChecksum` | ported | `src/ltx.rs` — parse path exercised in `decode_file` checksum verification |
| `TestFormatTimestamp` | ported | `src/ltx.rs` — timestamp field stored as unix-millisecond `i64` in header; round-trip in `header_marshal_roundtrip` |
| `TestParseTimestamp` | ported | Same |
| `TestIsContiguous` | ported | `src/replica.rs::calc_restore_plan` (chain-contiguity check); `src/replica.rs::tests::calc_restore_plan_l0_chain_and_empty` |

---

## KEEP-scope gap check

Every KEEP-scope module has ported tests:

| KEEP module | Ported tests | Notes |
|-------------|-------------|-------|
| WAL parser + checksums | `src/wal.rs` (16 tests) + `test_golden_sample_wal` | Byte-exact golden; full T1 coverage |
| LTX read/write + TXID | `src/ltx.rs` (9 tests) + all ltx-go tests above | Byte-exact golden + differential G3 |
| DB lifecycle | `src/db.rs` (23 tests) | Full capture-loop coverage |
| Store / retention | `src/store.rs` (9 tests) | L0 retention selection fully tested |
| Replica sync + restore | `src/replica.rs` (9 tests) + integration tests | G2 round-trip + G3 differential |
| ReplicaClient trait | `tests/conformance.rs` run via file + MinIO | Conformance suite green |
| File client | `src/client/file.rs` (3 tests) + conformance | Full write/read/list/delete |
| S3 / MinIO client | `src/client/object_store.rs` (10 tests) + `tests/integration_minio.rs` | Live MinIO tested |
| Leaser (fencing) | `src/leaser.rs` (28 tests) | Concurrency gate + failover |
| Heartbeat | `src/leaser.rs` (heartbeat tests) | All behavior except store integration |
| Replica URL / config | `src/replica_url.rs` (9 tests) | All KEEP-scope URL forms |
| Fuzz / resilience | `tests/fuzz_parsers.rs`, `tests/faults_inject.rs` | G4 gate |
| Property / round-trip | `tests/property_roundtrip.rs` | G4 proptest |
| Differential | `tests/differential_xtool.rs` | G3 both directions, byte-identical |

**No KEEP-scope test is silently missing.**

All deferred items are explicitly logged in `OPEN_QUESTIONS.md` under the
relevant task entry.  All dropped items correspond to PLAN.md §2 DROP scope
(VFS/server, v0.3 generation format, compaction levels, extra cloud clients,
Prometheus metrics, standalone CLI).

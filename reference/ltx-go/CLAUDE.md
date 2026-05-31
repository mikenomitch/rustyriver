# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build and Test Commands

```bash
# Run all tests
go test -v ./...

# Build the CLI tool
go build ./cmd/ltx

# Install the CLI tool
go install ./cmd/ltx

# Build with version info (for releases)
go build -ldflags "-s -w -X 'main.Version=<tag>' -X 'main.Commit=<sha>'" -o dist/ltx ./cmd/ltx
```

## Architecture Overview

LTX (Lite Transaction File) is a Go library and CLI for managing SQLite transactional data in an encrypted, compactable format. It stores SQLite page-level changes with checksums for replication and backup workflows.

### File Format (Version 3)

The LTX format has three sections:
1. **Header** (100 bytes) - Transaction metadata, page size, TXID range, timestamps, checksums
2. **Page Block** - LZ4-compressed page data with individual page headers
3. **Trailer** (16 bytes) - Post-apply checksum and file integrity checksum

### Core Components

- **ltx.go** - Core types: `Header`, `PageHeader`, `Trailer`, `TXID`, `Checksum`, `Pos`
- **encoder.go** - Encodes data into LTX format with LZ4 compression and rolling checksums
- **decoder.go** - Decodes LTX files with checksum verification
- **compactor.go** - Merges multiple LTX files into one compacted file
- **checksum.go** - CRC-ISO-64 checksum utilities

### CLI Commands (cmd/ltx/)

| Command | Purpose |
|---------|---------|
| `ltx apply -db PATH FILES...` | Apply LTX files to SQLite database |
| `ltx verify FILES...` | Verify LTX file integrity |
| `ltx list [-tsv] FILES...` | Display metadata for LTX files |
| `ltx dump PATH` | Dump all metadata and page info |
| `ltx checksum DB_PATH` | Compute CRC-ISO-64 checksum of database |
| `ltx encode-db DB OUTPUT.LTX` | Create LTX snapshot from database |

### Key Types

```go
type Pos struct {           // Replication position
    TXID              TXID
    PostApplyChecksum Checksum
}

type TXID uint64            // Transaction ID
type Checksum uint64        // CRC-ISO-64 checksum
```

### Testing Patterns

- Use `FileSpec` helper for creating in-memory test fixtures
- Round-trip tests: encode -> decode -> verify
- Table-driven tests for type parsing/marshaling

# Version Control

Always create feature branches. Never commit to main. Never do anything 
destructive without asking.
# matrix-archiver

Local incremental archiving of Matrix rooms

## Build

```
cargo build
```

## Configuration

Data directory must always be set, either via env var or `--data <PATH>`. No default.

```
export MATRIX_ARCHIVER_DATA=/path/to/dir
```

## Commands

Setup:

```
matrix-archiver login        # log in
matrix-archiver verify       # import E2E keys
```

Fetch:

```
matrix-archiver sync         # fetch messages + media
matrix-archiver decrypt      # decrypt archived events
```

Query:

```
matrix-archiver rooms        # list rooms
matrix-archiver users        # list users
matrix-archiver messages     # chat timeline
matrix-archiver events       # query events
matrix-archiver media <EID>  # output media bytes
```

## Data directory

```
$MATRIX_ARCHIVER_DATA/
  *.db             — matrix-sdk SQLite stores (state, crypto, event cache)
  archive.db       — message archive
  media/           — downloaded media files
    server.name/
      media_id         — original file
      media_id.thumb   — thumbnail
```

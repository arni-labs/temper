# ADR-0186: Search by Meaning

- Status: Accepted
- Date: 2026-11-17
- Deciders: Temper core maintainers
- Supersedes: ADR-0155 (declared vector access paths, and its rejection of kernel-computed embeddings)
- Related:
  - ADR-0153: Declared composite-key index (the co-commit and backfill pattern reused here)
  - `crates/temper-meaning` (the model, run in process)
  - `crates/temper-runtime/src/persistence/meaning.rs` (the record's text, the meaning row)
  - `crates/temper-server/src/meaning/` (model hook, worker, ranking)
  - `crates/temper-server/src/odata/query_plane_read/search.rs` (`$search`)
  - `crates/temper-server/src/odata/nearest.rs` (`Temper.Nearest`)

## Context

ADR-0155 let a spec declare a `[[vector]]` property that the kernel ranked by closeness. Every app
that wanted search had to compute embeddings itself, store them as state, declare them, and keep
them current; an app that did none of that could only be searched by exact `$filter` or substring.
So most records were not findable by what they mean, and the apps that were each carried their own
embedding code.

ADR-0155 kept embedding out of the kernel because it was a live call to a remote model: not
reproducible under simulation, and a network dependency in the write path. A small multilingual
model now runs in process on one CPU core, in about 90 MB, from weights pinned at build time. That
removes both objections: the model is a pure function of its pinned weights, and it runs after the
commit, never in it.

## Decision

Every record of every app is findable by meaning. Apps declare nothing; callers pass words.

### The model

`intfloat/multilingual-e5-small` at revision `614241f622f53c4eeff9890bdc4f31cfecc418b3`: a
12-layer BERT encoder (384 dimensions, ~118M parameters, a 250k-piece multilingual vocabulary).
`crates/temper-meaning` runs it in pure Rust: `build.rs` fetches the weights and tokenizer at the
pinned revision, checks their sha256, and packs them into the binary as f16. At load the encoder's
weights widen to f32 on the heap (86 MB); the vocabulary table stays in the binary. Its outputs
match the reference implementation (cosine ≥ 0.9999995 on a test corpus; identical tokenization).

The model reads a record's first 256 pieces (`MAX_PIECES`): one meaning per record. Texts get the
model's `passage: ` and `query: ` prefixes, mean pooling and unit length. Its tag,
`intfloat/multilingual-e5-small@<revision>`, is stored with every vector; vectors are compared only
within one tag, and a row made by another model waits for this one.

### The meaning row

Each store keeps one row per record, beside its query-plane row and written in the same commit
(`entity_meaning`, all three stores): the record's text, the text's hash, the model tag and the
vector. The text is one general rule, `record_text`: every non-empty string among the record's
query-plane fields, nested values included, one per line, without the fields the kernel writes
(id, status, event metadata). A field a spec marks `query_indexed = false` is not in the query
plane, so it is not searched either. A save whose text hashes the same leaves the row and its
vector alone; a new text clears the vector. A delete removes the row.

So a record is findable by its words the moment its save commits.

### The vector, just after the save

A worker (`meaning::worker`) waits on a wake that every append and backfill write rings, reads
the rows whose vector was not made by the current model, embeds them in batches of 16 on a
blocking thread, and keeps each vector only if its row still holds the text it was made from
(`set_meaning_vector` compares the hash). A record that changes again meanwhile waits for its new
text; a crash loses nothing, since a waiting row stays waiting. The model reads the text without
the words that hold a digit (ids, hashes, dates, numbers): they carry no meaning to it and would
fill its 256 pieces. Word match still finds them.

A backfill gives records saved before this change their rows, per type, and marks the type with
the text rule's version, so a later change to `record_text` re-derives every row once.

### Reading

`$search` on any entity set: records whose folded text (case and accents dropped) holds every
searched word come first, then every other record with a vector by closeness of meaning to the
words, then by id. `$filter`, `$select`, `$top`, `$skip`, `$count` and `$expand` apply as on any
read, under the same Cedar list gate and per-row read gate. Results carry `@temper.words` and
`@temper.score`. `$orderby` with `$search` is refused: the search is the order.

`Temper.Nearest(to='<id>',k=,filter=)` ranks a set's other records by closeness to one record's
meaning. Its `decl` and `model` arguments are accepted and ignored; `vector=` is refused.

### `[[vector]]` retires

The `[[vector]]` section, its table data and the `entity_vector_index` reads and writes are
deleted. The parser accepts and ignores a `[[vector]]` block, so specs written before this still
load. The old tables stay in existing databases, unused, so an older build can roll back onto them.

## Consequences

### Positive
- Every record of every app is findable by meaning, in about a hundred languages, with no app code.
- Apps delete their embedding code, their vector state and their `[[vector]]` declarations.
- Search is one read under the same authorization as every other read.

### Negative
- The binary carries the model: about 250 MB more (the f16 weights and vocabulary).
- Building `temper-meaning` fetches 470 MB from the model's host once per build cache.
- A Temper holds about 90 MB more heap once the model loads (measured on aarch64: 128 MB resident
  after load, 141 MB after use, against a few MB before).
- Embedding takes CPU after saves: on aarch64 about 10 ms per short record in a batch, 18 ms
  alone, 175 ms for a 256-piece record, on one core.
- Ranking reads every meaning row of a type per search. That is linear in the type's records,
  page by page; an approximate index is the next step when a type outgrows it.

### Risks
- A host short on memory: the model loads after boot, off the serving path, and a load failure
  leaves search ranking by words.
- A burst of saves queues vectors; records stay findable by words while they wait.

### DST Compliance
- Simulation-visible crates never run the model. `temper-server` defines `MeaningModel`; the
  binary (`temper-cli`) installs the real one; simulation installs `WordCountModel`, a
  deterministic stand-in.
- Ranking accumulates in f64 in the store's id order and breaks ties by id: the same order on
  every store and seed.
- The model's thread and timing live in `temper-cli` (`// determinism-ok`).

## Alternatives Considered

1. **Keep `[[vector]]` and let apps embed** — every app repeats embedding, and records of apps
   that do not are never found by meaning.
2. **A remote embedding service** — a network dependency and a cost per save; no better results
   for record-sized texts.
3. **Embed in the commit** — a save would wait for the model; after the commit, a save waits for
   nothing and the record is findable by words at once.
4. **One vector per field or per chunk** — more rows and more model time for little gain on
   record-sized texts; one meaning per record from its first 256 pieces.

## Rollback Policy

An older build ignores `entity_meaning` and finds its `entity_vector_index` tables as it left
them. Apps that dropped their `[[vector]]` blocks would have no vectors there until they write them
again.

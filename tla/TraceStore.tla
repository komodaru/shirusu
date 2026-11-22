----------------------------- MODULE TraceStore -----------------------------
(* Writing a Trace and reading it back after a crash (crates/shirusu-trace/src/writer.rs
   TraceWriter: add_payload, flush_blobs, write_block, finish; crates/shirusu-host/src/
   trace_pipeline.rs write_staged / absorb_storage_full; reader.rs, the recovering scan).

   Batches are written in order, each one Event Chunk of one event referring to one Blob (two
   events share a Blob: add_payload's dedup, by `known`). A batch first flushes the pending
   Blobs as a Blob Chunk, then writes its Event Chunk. A Block write succeeds, fails before its
   first byte, or fails after some bytes: then the Writer truncates the file back to the
   Block's start or, failing that, is torn and refuses every later Block (WriterInvariant). A
   failure is storage full (absorb_storage_full: the batch's unsaved event is counted dropped,
   every later batch is dropped unwritten, the pending Blobs stay pending, Finalization fits in
   the reserve) or another I/O error (the session fails: no Finalization). With SyncPerBlock a
   Block's sync may fail after it landed (TraceError::Sync: written; the session fails).
   finish flushes the pending Blobs (given up when storage is full), then writes Finalization
   with the dropped events and the Index (each Block's offset as the Writer counts them).
   Latch: the pipeline drops every batch after storage ran full; without it (a Writer whose
   user goes on writing, as when space came back) later batches are written as usual.

   A crash leaves a prefix of the file: anything from the last sync on (SyncPerBlock), any
   prefix at all (Buffered). The reader takes every whole Block in it and resyncs past a
   partial one.

   Guarantees (invariants; the read-back ones for every prefix a crash may leave):
     NoDuplicate   no event is in two Blocks
     SavedIsKept   an event told saved is in a whole Block (synced, with SyncPerBlock, unless
                   the session failed on that sync)
     Accounted     with Finalization read, every event is read or counted dropped, not both
     RefsResolve   every event read has its Blob read
     IndexAgrees   the Index read names the whole Blocks before it, where they are
   Variants (each must be violated; scripts/ci/tla.sh):
     TraceStoreNoTruncate.cfg   a partial Block left in place and written past (before P2-10):
                                IndexAgrees
     TraceStoreDropPending.cfg  without Latch, Blobs whose Chunk failed dropped from pending,
                                still known: RefsResolve (a later event of the same Blob
                                refers to nothing). With Latch the pending Blobs are never
                                written again, so the pipeline alone does not need them kept;
                                TraceStoreRetry.cfg (no Latch, kept) holds every guarantee.
     TraceStoreDoneOnly.cfg     "Finalization read => finish answered Ok", violated: a sync
                                failing after Finalization landed leaves a Trace that reads as
                                finished (whole: Accounted holds) while the Host was told it failed
     TraceStoreReachDegraded.cfg, TraceStoreReachTorn.cfg: a Trace finished with events both
                                saved and dropped, and a torn Writer, are reached
   Bounds: 3 events over 2 Blobs, at most MaxFail failed writes. *)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS Events, Ref, MaxFail, PerBlock, Truncate, KeepPending, Latch

\* (the configurations' Ref: two events of Blob "a", one of "b")
SharedRef == [e \in Events |-> IF e = 3 THEN "b" ELSE "a"]

N == Cardinality(Events)
\* the events in batch order
Order == [i \in 1..N |-> CHOOSE e \in Events : Cardinality({x \in Events : x <= e}) = i]

VARIABLES
  file,     \* the Blocks in the file: [kind, ids, whole, lost, index]
  woff,     \* how many Blocks the Writer counts in the file
  pending, known, told, next, phase, full, torn, st, synced, fails

vars == <<file, woff, pending, known, told, next, phase, full, torn, st, synced, fails>>

Init ==
  /\ file = <<>> /\ woff = 0 /\ pending = {} /\ known = {}
  /\ told = [e \in Events |-> "none"]
  /\ next = 1 /\ phase = "idle" /\ full = FALSE /\ torn = FALSE /\ st = "run"
  /\ synced = 0 /\ fails = 0

Block(kind, ids) == [kind |-> kind, ids |-> ids, whole |-> TRUE, lost |-> {}, index |-> {}]

(* a write of block b: the outcomes, as [res, file, woff, torn, synced, fails]; res "ok",
   "sync" (landed, its sync failed), "full" or "io" (failed). Finalization is never refused
   for space (the reserve). *)
Writes(b) ==
  LET at == woff + 1
      ok == [res |-> "ok", file |-> Append(file, b), woff |-> at, torn |-> torn,
             synced |-> IF PerBlock THEN Len(file) + 1 ELSE synced, fails |-> fails]
      sync == [ok EXCEPT !.res = "sync", !.synced = synced]
      none(r) == [res |-> r, file |-> file, woff |-> woff, torn |-> torn, synced |-> synced, fails |-> fails + 1]
      \* some bytes landed: truncated back (none), or left in place
      part(r) == [res |-> r, file |-> Append(file, [b EXCEPT !.whole = FALSE]), woff |-> woff,
                  torn |-> Truncate, synced |-> synced, fails |-> fails + 1]
      kinds == IF b.kind = "fin" THEN {"io"} ELSE {"full", "io"}
  IN IF torn THEN {none("io")}
     ELSE {ok} \cup (IF PerBlock THEN {sync} ELSE {})
          \cup (IF fails < MaxFail THEN UNION {{none(r), part(r)} : r \in kinds} ELSE {})

Take(w) == file' = w.file /\ woff' = w.woff /\ torn' = w.torn /\ synced' = w.synced /\ fails' = w.fails
Keep == UNCHANGED <<file, woff, torn, synced, fails>>

\* the next batch: dropped unwritten once storage ran full; else its Blob made known
Start ==
  /\ st = "run" /\ phase = "idle" /\ next <= N
  /\ LET e == Order[next] IN
       IF full /\ Latch
       THEN told' = [told EXCEPT ![e] = "dropped"] /\ next' = next + 1
            /\ UNCHANGED <<pending, known, phase>>
       ELSE /\ known' = known \cup {Ref[e]}
            /\ pending' = IF Ref[e] \in known THEN pending ELSE pending \cup {Ref[e]}
            /\ phase' = "blob" /\ UNCHANGED <<told, next>>
  /\ UNCHANGED <<full, st>> /\ Keep

\* a failure that ends the batch: storage full drops its event, anything else the session
Failed(w, e) ==
  /\ told' = [told EXCEPT ![e] = IF w.res = "full" THEN "dropped" ELSE told[e]]
  /\ full' = (full \/ w.res = "full")
  /\ st' = IF w.res = "full" THEN "run" ELSE "failed"
  /\ phase' = "idle" /\ next' = next + 1

BlobChunk ==
  /\ st = "run" /\ phase = "blob"
  /\ LET e == Order[next] IN
       IF pending = {}
       THEN phase' = "event" /\ UNCHANGED <<pending, told, next, full, st>> /\ Keep
       ELSE \E w \in Writes(Block("blob", pending)) :
              /\ Take(w)
              /\ CASE w.res = "ok" -> pending' = {} /\ phase' = "event" /\ UNCHANGED <<told, next, full, st>>
                   [] w.res = "sync" -> pending' = {} /\ st' = "failed" /\ UNCHANGED <<told, next, full, phase>>
                   [] OTHER -> pending' = (IF KeepPending THEN pending ELSE {}) /\ Failed(w, e)
  /\ UNCHANGED known

EventChunk ==
  /\ st = "run" /\ phase = "event"
  /\ LET e == Order[next] IN
       \E w \in Writes(Block("event", {e})) :
         /\ Take(w)
         /\ CASE w.res = "ok" -> told' = [told EXCEPT ![e] = "saved"] /\ phase' = "idle" /\ next' = next + 1 /\ UNCHANGED <<full, st>>
              [] w.res = "sync" -> told' = [told EXCEPT ![e] = "saved"] /\ st' = "failed" /\ UNCHANGED <<phase, next, full>>
              [] OTHER -> Failed(w, e)
  /\ UNCHANGED <<pending, known>>

\* finish: the pending Blobs (given up once storage is full and they do not fit), Finalization
FinishBlobs ==
  /\ st = "run" /\ phase = "idle" /\ next > N /\ pending # {}
  /\ \E w \in Writes(Block("blob", pending)) :
       /\ Take(w)
       /\ CASE w.res = "ok" -> pending' = {} /\ UNCHANGED st
            [] w.res = "full" /\ full -> pending' = {} /\ UNCHANGED st
            [] OTHER -> st' = "failed" /\ UNCHANGED pending
  /\ UNCHANGED <<known, told, next, phase, full>>

Finish ==
  /\ st = "run" /\ phase = "idle" /\ next > N /\ pending = {}
  /\ LET index == {p \in 1..woff : TRUE}
         fin == [Block("fin", {}) EXCEPT !.lost = {e \in Events : told[e] = "dropped"}, !.index = index]
     IN \E w \in Writes(fin) :
          /\ Take(w)
          /\ st' = IF w.res = "ok" THEN "done" ELSE "failed"
  /\ UNCHANGED <<pending, known, told, next, phase, full>>

Next == Start \/ BlobChunk \/ EventChunk \/ FinishBlobs \/ Finish

Spec == Init /\ [][Next]_vars

\* --- reading back ------------------------------------------------------------------------

Prefixes == IF PerBlock THEN synced..Len(file) ELSE 0..Len(file)
Whole(d) == {i \in 1..d : file[i].whole}
Read(d, kind) == UNION {file[i].ids : i \in {j \in Whole(d) : file[j].kind = kind}}
Fins(d) == {i \in Whole(d) : file[i].kind = "fin"}

NoDuplicate ==
  \A i, j \in 1..Len(file) :
    (i # j /\ file[i].whole /\ file[j].whole /\ file[i].kind = "event" /\ file[j].kind = "event")
      => file[i].ids \cap file[j].ids = {}

SavedIsKept ==
  \A e \in Events : told[e] = "saved" =>
    \E p \in 1..Len(file) : file[p].whole /\ file[p].kind = "event" /\ e \in file[p].ids
                            /\ (PerBlock => p <= synced \/ st = "failed")

Accounted ==
  \A d \in Prefixes : \A i \in Fins(d) :
    Read(d, "event") \cup file[i].lost = Events /\ Read(d, "event") \cap file[i].lost = {}

RefsResolve == \A d \in Prefixes : \A e \in Read(d, "event") : Ref[e] \in Read(d, "blob")

IndexAgrees ==
  \A d \in Prefixes : \A i \in Fins(d) :
    file[i].index = {p \in 1..(i - 1) : file[p].whole}

\* (states that must be reached, so the guarantees are not vacuous: each is a violation)
NoDegradedFinish == ~(st = "done" /\ (\E e \in Events : told[e] = "dropped") /\ (\E e \in Events : told[e] = "saved"))
NeverTorn == ~torn

DoneOnly == \A d \in Prefixes : Fins(d) # {} => st = "done"
=============================================================================

---------------------------- MODULE RuntimeHazard ----------------------------
(* Sync contract between Detours, the RuntimeSlot and shutdown (review P2):
   crates/shirusu-hook/src/runtime.rs and crates/shirusu-hook/src/hazard.rs.

   One runtime R is followed through its whole life:
     active (revision 1) -> unregister: recording gate closed, retired
     -> optional drain wait (wait_in_flight_drained)
     -> register R2 (revision 2) -> unregister R2: R moves to the graveyard
     -> reclaim: heavy fence, scan hazards, drop the slot's Arc if none.

   A Detour enters R in one of two ways.
   - Hazard path (warm cache, RuntimeSlot::cached_hazard + begin_ref): load
     the revision, publish the hazard with a plain store and a compiler
     fence only, re-load the revision, then dereference R to check the gate
     and record. Clearing the hazard is a plain (Release) store.
   - Shared path (for_detour_ref slow path + begin_ref): take an Arc under
     the read lock, check the gate, fetch_add in_flight, re-check the gate,
     record, fetch_sub, drop the Arc.

   Memory model as in AsymmetricHandshake.tla: the hazard stores of a
   Detour go through a per-thread FIFO store buffer (x86 TSO store->load
   reordering); its loads read memory (it never loads its own hazard here).
   Locked RMWs (in_flight, lock acquisition) drain the buffer. The heavy
   fence (FlushProcessWriteBuffers) drains every buffer. Controller stores
   are SeqCst or under the write lock and go straight to memory.

   Properties:
   - HazardSafe: no Detour dereferences R after it was freed.
   - DrainedQuiet: once the drain wait reported zero, no Detour records.
   - Reclaimed: the slot eventually drops R (no hazard leaks forever).

   Variants (each must be refuted by TLC, see scripts/ci/tla.sh):
   - RevisionRecheck = FALSE: no revision re-load after publishing.
   - HeavyReclaim = FALSE: reclaim scans hazards without the heavy fence.
   - HeavyWait = FALSE: the first drain check skips the heavy fence.
   - GateRecheck = FALSE: the shared path does not re-check the gate after
     fetch_add.
*)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS Threads, Rounds, RevisionRecheck, HeavyReclaim, HeavyWait, GateRecheck

VARIABLES hz, buf, pc, rounds, arc, inflight, gate, rev, where, cpc, drained

vars == <<hz, buf, pc, rounds, arc, inflight, gate, rev, where, cpc, drained>>

Pcs == {"idle", "f_rev1", "f_pub", "f_rev2", "f_gate", "f_rec", "f_clear",
        "s_lock", "s_gate1", "s_add", "s_gate2", "s_rec", "s_sub", "s_drop",
        "done"}

Freed == where = "dropped" /\ \A t \in Threads : ~arc[t]

DrainAll == /\ hz' = [t \in Threads |->
                        IF buf[t] = <<>> THEN hz[t] ELSE buf[t][Len(buf[t])]]
            /\ buf' = [t \in Threads |-> <<>>]

DrainOwn(t) == /\ hz' = [hz EXCEPT ![t] = IF buf[t] = <<>> THEN hz[t]
                                          ELSE buf[t][Len(buf[t])]]
               /\ buf' = [buf EXCEPT ![t] = <<>>]

\* in_flight() as the code computes it: counter plus hazards it can see.
Count(h) == inflight + Cardinality({t \in Threads : h[t]})

Init ==
  /\ hz = [t \in Threads |-> FALSE]
  /\ buf = [t \in Threads |-> <<>>]
  /\ pc = [t \in Threads |-> "idle"]
  /\ rounds = [t \in Threads |-> 0]
  /\ arc = [t \in Threads |-> FALSE]
  /\ inflight = 0
  /\ gate = TRUE
  /\ rev = 1
  /\ where = "active"
  /\ cpc = "unreg1"
  /\ drained = FALSE

Go(t, p) == pc' = [pc EXCEPT ![t] = p]
Finish(t) == /\ rounds' = [rounds EXCEPT ![t] = @ + 1]
             /\ pc' = [pc EXCEPT ![t] = IF rounds[t] + 1 = Rounds THEN "done" ELSE "idle"]
Push(t, v) == buf' = [buf EXCEPT ![t] = Append(@, v)]

\* ---- Detour (light side) ----
Start(t) == /\ pc[t] = "idle"
            /\ \/ Go(t, "f_rev1")
               \/ Go(t, "s_lock")
            /\ UNCHANGED <<hz, buf, rounds, arc, inflight, gate, rev, where, cpc, drained>>

\* The cache was filled under revision 1 for R; another revision means the
\* fast path is not taken for R (the Detour uses R2 or the slow path).
FRev1(t) == /\ pc[t] = "f_rev1"
            /\ IF rev = 1 THEN Go(t, "f_pub") /\ UNCHANGED rounds ELSE Finish(t)
            /\ UNCHANGED <<hz, buf, arc, inflight, gate, rev, where, cpc, drained>>

FPub(t) == /\ pc[t] = "f_pub"
           /\ Push(t, TRUE)
           /\ Go(t, IF RevisionRecheck THEN "f_rev2" ELSE "f_gate")
           /\ UNCHANGED <<hz, rounds, arc, inflight, gate, rev, where, cpc, drained>>

FRev2(t) == /\ pc[t] = "f_rev2"
            /\ Go(t, IF rev = 1 THEN "f_gate" ELSE "f_clear")
            /\ UNCHANGED <<hz, buf, rounds, arc, inflight, gate, rev, where, cpc, drained>>

\* begin_ref: dereferences R to read its gate.
FGate(t) == /\ pc[t] = "f_gate"
            /\ Go(t, IF gate THEN "f_rec" ELSE "f_clear")
            /\ UNCHANGED <<hz, buf, rounds, arc, inflight, gate, rev, where, cpc, drained>>

FRec(t) == /\ pc[t] = "f_rec"
           /\ Go(t, "f_clear")
           /\ UNCHANGED <<hz, buf, rounds, arc, inflight, gate, rev, where, cpc, drained>>

FClear(t) == /\ pc[t] = "f_clear"
             /\ Push(t, FALSE)
             /\ Finish(t)
             /\ UNCHANGED <<hz, arc, inflight, gate, rev, where, cpc, drained>>

\* Read lock (a full barrier): R is returned while it is active, or retired
\* with no successor registered yet.
SLock(t) == /\ pc[t] = "s_lock"
            /\ DrainOwn(t)
            /\ IF where = "active" \/ (where = "retired" /\ rev = 1)
               THEN /\ arc' = [arc EXCEPT ![t] = TRUE]
                    /\ Go(t, "s_gate1") /\ UNCHANGED rounds
               ELSE /\ Finish(t) /\ UNCHANGED arc
            /\ UNCHANGED <<inflight, gate, rev, where, cpc, drained>>

SGate1(t) == /\ pc[t] = "s_gate1"
             /\ Go(t, IF gate THEN "s_add" ELSE "s_drop")
             /\ UNCHANGED <<hz, buf, rounds, arc, inflight, gate, rev, where, cpc, drained>>

SAdd(t) == /\ pc[t] = "s_add"
           /\ DrainOwn(t)
           /\ inflight' = inflight + 1
           /\ Go(t, IF GateRecheck THEN "s_gate2" ELSE "s_rec")
           /\ UNCHANGED <<rounds, arc, gate, rev, where, cpc, drained>>

SGate2(t) == /\ pc[t] = "s_gate2"
             /\ Go(t, IF gate THEN "s_rec" ELSE "s_sub")
             /\ UNCHANGED <<hz, buf, rounds, arc, inflight, gate, rev, where, cpc, drained>>

SRec(t) == /\ pc[t] = "s_rec"
           /\ Go(t, "s_sub")
           /\ UNCHANGED <<hz, buf, rounds, arc, inflight, gate, rev, where, cpc, drained>>

SSub(t) == /\ pc[t] = "s_sub"
           /\ DrainOwn(t)
           /\ inflight' = inflight - 1
           /\ Go(t, "s_drop")
           /\ UNCHANGED <<rounds, arc, gate, rev, where, cpc, drained>>

\* Dropping the last Arc frees R (Freed is derived from where and arc).
SDrop(t) == /\ pc[t] = "s_drop"
            /\ arc' = [arc EXCEPT ![t] = FALSE]
            /\ Finish(t)
            /\ UNCHANGED <<hz, buf, inflight, gate, rev, where, cpc, drained>>

Commit(t) == /\ buf[t] # <<>>
             /\ hz' = [hz EXCEPT ![t] = Head(buf[t])]
             /\ buf' = [buf EXCEPT ![t] = Tail(@)]
             /\ UNCHANGED <<pc, rounds, arc, inflight, gate, rev, where, cpc, drained>>

\* ---- Controller (heavy side) ----
CUnreg1 == /\ cpc = "unreg1"
           /\ gate' = FALSE
           /\ where' = "retired"
           /\ cpc' \in {"wait_heavy", "reg2"}
           /\ UNCHANGED <<hz, buf, pc, rounds, arc, inflight, rev, drained>>

\* wait_in_flight_drained: the first check pays the heavy fence, the later
\* ones read published hazards only (hazard::count_published).
CWaitHeavy == /\ cpc = "wait_heavy"
              /\ IF HeavyWait THEN DrainAll ELSE UNCHANGED <<hz, buf>>
              /\ IF Count(IF HeavyWait THEN hz' ELSE hz) = 0
                 THEN drained' = TRUE /\ cpc' = "reg2"
                 ELSE cpc' = "wait_light" /\ UNCHANGED drained
              /\ UNCHANGED <<pc, rounds, arc, inflight, gate, rev, where>>

CWaitLight == /\ cpc = "wait_light"
              /\ Count(hz) = 0
              /\ drained' = TRUE
              /\ cpc' = "reg2"
              /\ UNCHANGED <<hz, buf, pc, rounds, arc, inflight, gate, rev, where>>

CReg2 == /\ cpc = "reg2"
         /\ rev' = 2
         /\ cpc' = "unreg2"
         /\ UNCHANGED <<hz, buf, pc, rounds, arc, inflight, gate, where, drained>>

CUnreg2 == /\ cpc = "unreg2"
           /\ where' = "grave"
           /\ cpc' = "reclaim"
           /\ UNCHANGED <<hz, buf, pc, rounds, arc, inflight, gate, rev, drained>>

\* hazard::count: heavy fence then scan; R stays in the graveyard (and is
\* retried by a later reclaim) while any hazard publishes it.
CReclaim == /\ cpc = "reclaim"
            /\ IF HeavyReclaim THEN DrainAll ELSE UNCHANGED <<hz, buf>>
            /\ LET h == IF HeavyReclaim THEN hz' ELSE hz IN
                 IF \A t \in Threads : ~h[t]
                 THEN where' = "dropped" /\ cpc' = "end"
                 ELSE UNCHANGED <<where, cpc>>
            /\ UNCHANGED <<pc, rounds, arc, inflight, gate, rev, drained>>

Thread(t) == Start(t) \/ FRev1(t) \/ FPub(t) \/ FRev2(t) \/ FGate(t) \/ FRec(t)
             \/ FClear(t) \/ SLock(t) \/ SGate1(t) \/ SAdd(t) \/ SGate2(t)
             \/ SRec(t) \/ SSub(t) \/ SDrop(t)

Controller == CUnreg1 \/ CWaitHeavy \/ CWaitLight \/ CReg2 \/ CUnreg2 \/ CReclaim

Next == Controller \/ \E t \in Threads : Thread(t) \/ Commit(t)

Spec == Init /\ [][Next]_vars
        /\ WF_vars(Controller)
        /\ \A t \in Threads : WF_vars(Thread(t)) /\ WF_vars(Commit(t))

TypeOK ==
  /\ pc \in [Threads -> Pcs]
  /\ hz \in [Threads -> BOOLEAN]
  /\ arc \in [Threads -> BOOLEAN]
  /\ inflight \in 0..Cardinality(Threads)
  /\ where \in {"active", "retired", "grave", "dropped"}

HazardSafe == Freed => \A t \in Threads : pc[t] \notin {"f_gate", "f_rec"}

DrainedQuiet == drained => \A t \in Threads : pc[t] \notin {"f_rec", "s_rec"}

Reclaimed == <>(where = "dropped")
=============================================================================

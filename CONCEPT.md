# Concept

ryo-v2 takes a change to a Rust workspace, written as a TypeSpec change or as a mutation list,
works out the procedure of commands that makes it, applies it automatically, and returns several
results that pass verification and meet the input. A command is a mutation of the code:
rust-analyzer's, or one ryo-v2 adds. This file is the concept every design decision
rests on. A decision that contradicts it changes the concept and is decided as such; everything it
leaves open is design freedom, settled by measurement. Where code, a ledger decision or another
document disagrees with this file, they are what is wrong.

## Execution model

```
  In: TypeSpec change | mutation list | free text + base mutation list
      (+ policies, expected next changes, deadline)
              |
Decision AI --+ seeds the first commands; with free text, guards each step against it
              v
+-- Speculation graph: state = base + commands applied, one node per state fingerprint --+
|   grows by expansion: rust-analyzer's commands and ryo-v2's own                        |
+-------------+-------------------------------------------------------^------------------+
              | work (expand / judge / check / check together)        | results (events)
              v                                                       |
        Scheduler (pure rules: value / cost, deadline, stop rule, speculation)
              |
     +--------+----------------+--------------------+----------------+
     v                         v                    v                v
   Forks: N live (4-16 *),   Expander:            Judge:           Assessor:
   writes stack on the       rust-analyzer's +    Decision AI,     static metrics
   parent, nested forks;     ryo-v2's commands,   priors, batched
   check = diagnostics +     one per pattern;
   TypeSpec relations        read-only, parallel
     |
     v
   Verdict cache: valid while its read set is unchanged (OCC); a base change invalidates
   only what read it
              |
              v
Out: the passing results (about ten *), ranked        Writer (basis -> splice -> re-check)
Idle: warm the cache for expected changes;                ^ writes the picked one
      when the base moves, re-evaluate what it touched    |
- - - - - - - - - - - - - - - - - - - - - - - - - - - - - | - - - - - - - - - - ryo-v2 | caller
Caller (Main AI or a person) picks one -------------------+
     -> the caller commits -> CI builds once

* hypothesis: a figure from the simulation, the scale the model has to reach; not yet measured
```

- **Search:** Monte Carlo over the speculation graph. A step applies a command. A state that passes
  (no error added) and meets the input (the TypeSpec holds, or the mutation list is carried out)
  is a result; the reward ranks results by the static metrics (clean code, extensibility against
  the expected next changes), less the bytes written.
- **Priority** of a piece of work = P(pass) × estimated reward / estimated cost. P(pass) comes from
  the static metrics and the Decision AI's prior, the cost from the files a check diagnoses.
  Checks start from the static score without waiting for priors, which reorder them on arrival.
- **Stop:** at the deadline, or when ten results have passed and no remaining bound beats the
  tenth (branch and bound).
- **Writes stack:** a child is written on top of its parent, never rebuilt from the base: a write
  is a rust-analyzer edit (milliseconds) and its check is incremental (seconds). This is the first
  condition of the model; N forks then share the cores.
- **Expected next changes** (probes): the changes a maintainer would plausibly make next (add a
  variant, a field, a method). Extensibility is how few edits each would need in a state; idle
  time checks them ahead to warm the cache.

### Example: a return type becomes `Result`

```
TypeSpec change: type Loaded = Spec<ConfigGroup, Config>;              (fn load() -> Loaded)
         becomes  type Loaded = Spec<ConfigGroup, Result<Config, LoadError>>;

base
 +- cmd: change the alias, wrap the returns of load in Ok(..)   Expander (rust-analyzer assist)
    s1: written on a fork; check -> 25 callers fail            Fork, check
    |   the choice the TypeSpec leaves: how a caller handles LoadError
    |   Expander generates one pattern per way it knows; Scheduler orders them
    |   by P(pass) x reward / cost; each child is written on top of s1
    +- pattern A: propagate with `?` at every caller
    |   s2a: check -> 12 callers of callers fail -> propagate again
    |      s3a: pass, TypeSpec holds
    +- pattern B: `unwrap` at every caller
    |   s2b: pass, TypeSpec holds
    +- pattern C: `match` to a default at every caller
        s2c: check -> Config has no default -> error, dropped

Out: s3a and s2b (each about ten thousand lines), ranked by reward
     each decides what happens when load fails: which one is wanted is the caller's choice
     a policy "propagate errors" handed with the input keeps s3a only
```

## Use case

A change large enough that making it by hand means many coordinated edits: an AI's expansion of an
issue, or a change to a workspace's own design. It is handed to ryo-v2 as a change of the
workspace's TypeSpec: its specification written in its own code as type markers, a set of
declarations and the specification they determine. ryo-v2 compares the code with the changed
TypeSpec, composes a procedure of available commands (rust-analyzer's, plus ryo-v2's own) that
leads there, applies it, explores alternative procedures, and returns the results that pass
verification and satisfy the TypeSpec. The caller picks one and commits it; CI builds once.

Where no TypeSpec covers the change, the input is the procedure itself: a mutation list (a chain of
rust-analyzer's and ryo-v2's commands), or free text with a base mutation list. ryo-v2 then varies
the base list step by step and optimises it by the same Monte Carlo search, the Decision AI
guarding each step against the text.

rust-analyzer runs a command at a place; which commands, in which order and where, bring the code
to the input, and whether the result compiles, is what ryo-v2 decides, and is otherwise manual.

The goal (hypothesis: the scale wanted, from the simulation, not yet measured): about ten passing
results within about a minute, also for a change of about ten thousand lines, 5 % of the files of
a 200,000-line workspace.

## Core

1. **The TypeSpec, where there is one, is the goal and the judge.** The search aims at the changed
   TypeSpec, and a result counts only when the code satisfies it: the compiler accepts it and
   ryo-v2's static analysis finds the relations it states held. The search never changes the
   TypeSpec beyond the change handed over; the code outside it is free. Without a TypeSpec the
   mutation list is the starting point; the Decision AI's guard decides what is tried, and
   verification what passes.
2. **Commands, not generated code.** A step applies a command. Where the TypeSpec leaves a choice
   (how a caller handles a new error, which value a new field takes), each pattern ryo-v2 knows is a
   command, and every variant that passes before the stop is returned as an alternative; a policy
   handed over with the input narrows the patterns tried. Only what no pattern can produce (a new
   body) is left as a hole.
3. **Verified as written.** A result is checked in exactly the text it would write. The truth of
   that check is `cargo check --workspace --all-targets`, and a result passes when it adds no error
   to the base. A file the check cannot judge makes a failure with its reason, never a result. A
   class of error the in-process check cannot see is named on the result; the build after commit
   settles it, and the rate at which it refuses a result is measured.
4. **Fast enough to search.** Verification runs in-process on a resident rust-analyzer, so a check
   takes seconds (the limit: 5 s or less per candidate on a warm session, a hypothesis) and many
   procedures can be compared. No `cargo`, `rustc` or test runs for a
   candidate (loading a base runs what rust-analyzer needs, once per base); the build runs once,
   after commit. The cost of a check follows what the change affects, not the size of the
   repository.
5. **The caller owns intent and choice.** ryo-v2 picks no winner: it returns alternatives, ranked.
   Free text reaches the search only through the Decision AI, as a guard on what is tried. An AI
   inside ryo-v2 may seed, order and prune the search; only verification decides what passes.

## Boundary

| axis | inside ryo-v2 | outside | set by |
|---|---|---|---|
| input | a TypeSpec change; where none covers the change, a mutation list, or free text with a base mutation list; policies that narrow the patterns, the expected next changes, a deadline | free text as the judge of what passes | 1, 2, 5 |
| what it makes | changes reachable by a sequence of commands, one variant per pattern that passes | new bodies (holes) | 2; the command catalog |
| what it decides | no error added to the base, and the input is met (the TypeSpec holds, or the mutation list is carried out) | behaviour (tests in CI), what the in-process check cannot see (named, settled by the build after commit), the choice among results | 1, 3, 4 |
| what it writes | the picked result, when its basis has no drift, spliced onto the current text and checked again | anything unverified; the commit (the caller's) | 3, 5 |

## Open: the form of the input

Whether a TypeSpec change is enough for ryo-v2 to find the procedure on its own, or the caller must
also hand over a mutation list, is settled by implementing both and measuring which changes each
resolves. Either form is inside this concept.

## How claims are judged

| claim | truth |
|---|---|
| verification | `cargo check --workspace --all-targets` on the changed workspace (the oracle, `CHECK_LEDGER.md`) |
| search | a known target: a change whose resulting code is known, handed over in one of the input forms, reached when that code is among the returned results (`SEARCH_LEDGER.md`) |
| after commit | the rate at which the build after commit refuses a returned result |

A metric of ryo-v2's own ranks results. It is never the truth a claim is judged against.

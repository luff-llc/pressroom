# Placing work across machines

**Status:** the first slice is built (`contracts/job-v1.md`). The hosted tier (§5) and the
pull wire (§4) are designed, not built. This follows the device-capacity design in the
Galley repository (`docs/design/2026-11-15-device-capacity.md`, §3 "Placing work"), which
planned placement across machines and run-work grants signed with the system's keys.

## 1. What it is for

An agent that needs a build or a test run should not choose a machine, open a shell on it,
copy its folder there and remember which Xcode it has. It asks Pressroom to run one named
kind of work on one exact tree, and gets the results back. Pressroom picks the machine.

Machines come in tiers:

| Tier | What | Reached | Capacity |
|---|---|---|---|
| **This machine** | The one the agent runs on | directly | measured by its daemon |
| **Owned machines** | A person's other machines: a server, a spare Mac, a Linux box | SSH now; later they pull (§4) | measured by each one's daemon |
| **Hosted** | Rented runners, by the minute | the provider's API (§5) | declared per runner type; elastic; costs money |

Each tier is a different way of reaching a machine and of knowing its capacity. The job
contract is the same on all three. That is the abstraction: a request, a recipe, a tree.

## 2. The job contract

A request names a recipe (repository and kind), a tree hash, parameter values, a class
and who is asking. A recipe is a file the machine's owner installed, holding the command.
The request fills in its holes, each value checked against a pattern and passed as one
argument with no shell. So a request can never carry a command, and a machine runs only
what its owner wrote down.

The source is a git tree, sent as the objects the machine lacks. A tree is content
addressed, so the machine knows it has exactly what was asked for, and two placers that
send the same tree send it once. The machine checks it out into the job's own lane,
rewriting only files that changed, so a warm build cache stays warm across worktrees and
across placers.

Results are a folder per job: the log, a small result record, and whatever the recipe
wrote to `{results}`. The placer fetches it whole. The job does not depend on the
connection that placed it.

Rules J1 to J13 are the detail.

## 3. Placement, capacity and disk floors

**Placement is advisory; admission is the authority.** Each machine already admits work
by weight against what its owner offers (`local-v1.md` rules 1 to 7): threads, memory, a
share kept for its person, outside load taken from that share first, a disk floor. A
placer reads those numbers from every machine at once and chooses, but it does not
reserve anything. The machine it chose admits the job by its own rules, so a stale picture
or two placers choosing the same machine cost a wait, never an overload. This is why no
central scheduler is needed for owned machines: the per-machine gate already exists, and
a second source of truth about capacity would disagree with it.

**The choice** (rule J9) rules machines out with a reason each (unreachable, no recipe,
wrong tool version, held by its person, too little memory, would pass its disk floor),
then prefers, in order: starts now; spares a person at their own machine; already has the
tree; most free threads. The reasons are printed, so a person or an agent can see why the
work went where it did.

**Tool versions are needs, not preferences.** A recipe can say `needs xcode = 27.1`, and a
request can add `--needs xcode=27.2`. A machine without that version is passed over, and
refuses the job if it is sent anyway. Work never runs on a toolchain it was not written
for. Aligning machines is the owner's job; Pressroom only makes a mismatch visible.

**Disk floors are checked twice.** The placer passes over a machine whose free disk, less
the recipe's `disk_gb`, is below its floor; the machine checks again on arrival; and local
admission keeps holding new work below the floor. A recipe's `disk_gb` is the most the job
may write, a promise the owner makes when writing it; it is not yet enforced while the job
runs.

## 4. The wire, now and next

**Now:** the placer runs `pressroom job ...` on the machine over SSH. Every word of that
command is fixed or a checked id. An `authorized_keys` forced command (`pressroom job
ssh`, rule J13) narrows the key to those verbs, so the key that places work cannot open a
shell.

**Next:** as the device-capacity design plans, a machine's daemon dials out and pulls
work, and each job carries a run-work grant signed with the system's keys, checked on the
machine when the job is taken and again when it starts. The request format does not
change: a pulled job is the same request line and the same pack, fetched instead of
pushed. Only `machines.conf` and the transport change.

## 5. How a hosted tier plugs in

A hosted runner is a machine that exists only for one job. Three things differ from an
owned machine, and each has a place in the design.

**Capacity is declared, not measured.** There is no daemon to probe before the job. The
placer reads a record per runner type instead, kept beside `machines.conf`:

```
machine hosted-mac = hosted actions owner/repo runner-label
hosted-mac threads = 6
hosted-mac memory_gb = 14
hosted-mac os = macos-26
hosted-mac tool xcode = 27.2
hosted-mac price_per_minute = 0.08
hosted-mac data = public
```

The tool versions come from the provider's published image for that label, pinned, so
rule J7 works unchanged.

**It is chosen last, and only for a reason.** A hosted machine takes the work when no
owned machine can (it needs an operating system or a tool version none of them has), or
when every owned machine would wait longer than a set time and the work is `waiting` or
`gate`. It is never chosen for `batch` work an owned machine could do later. Each hosted
machine has a monthly budget; past it, it is ruled out with that reason. Owned machines
cost their idle power whether they work or not, so filling them first is also the cheap
choice.

**Data has a ceiling.** Each hosted machine declares the highest class of source it may
receive (`public` by default; `private` only when its owner raises it). A repository's
class is declared by its owner. The device-capacity design (§6) has the classes.

**The job runs as it would on an owned machine.** The provider starts a runner from a
workflow kept in the consumer's repository, on a reviewed branch: it installs a pinned
Pressroom release, reads its recipes from that branch (never from the job's tree, so rule
J2 holds), fetches the job's tree, and runs `pressroom job run` with the same request line
and `pack: false`. The tree reaches it as a ref the placer pushes to the forge
(`refs/pressroom/<tree>`, a commit made for the tree and nothing else), since a hosted
runner cannot receive a push. Results go back as the run's artifact, and the placer
fetches them into `--results` as it does from any machine. Credentials to start a run are
held by the placer's owner (later, the broker), never by the job.

**What is built for it already:** the request format, recipes, rule J7's version check,
the choice with reasons, results as a folder, and `machines.conf` refusing `hosted` with a
pointer here rather than ignoring it.

## 6. Order of work

| Slice | Delivers |
|---|---|
| **1 (built)** | `place` and the machine's `job` verbs over SSH or directly; recipes; trees sent as missing objects; checkout into lanes; the choice with reasons; one consumer's host test scripts on an owned server |
| 2 | More kinds: tests run from products built once (the products sent like a tree, by hash), and a recipe that holds a device slot (local rule 27) |
| 3 | The hosted tier (§5), behind a budget and a data ceiling |
| 4 | The pull wire and grants (§4) |

## 7. Rejected

- **A central queue that owns capacity.** It would be a second, staler copy of what each
  machine's gate already knows.
- **Sending a command, or a script, with the request.** Then a machine runs whatever any
  caller sends. Recipes keep the owner in charge of what runs.
- **Copying the folder with rsync.** It sends the working folder, not a named tree; a
  machine cannot tell what it was given, and two callers' copies collide.
- **One checkout per job.** A cold build every time, and a full copy of the source on disk
  for every job.

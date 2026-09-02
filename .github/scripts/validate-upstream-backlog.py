#!/usr/bin/env python3
"""Validate .github/upstream-backlog.toml.

The backlog moved out of a GitHub issue body and into this repository so that a
change to it is a reviewable diff rather than an agent rewrite or a stray click
on a checkbox. That only buys anything if something mechanically checks the file
is still true, which is what this is.

Two classes of check:

  STRUCTURE   the file is well-formed and internally consistent. Always runs.
  WATERMARK   the file agrees with the submodule pin, which is the real source
              of truth for what is outstanding. Runs only when the submodule is
              checked out; CI checks it out recursively, so CI always runs it.

Exit 0 on success, 1 on any error. Warnings never fail the build.

Usage:
    python3 .github/scripts/validate-upstream-backlog.py [path]
"""
import re
import subprocess
import sys
import tomllib
from pathlib import Path

BACKLOG = Path(sys.argv[1] if len(sys.argv) > 1 else ".github/upstream-backlog.toml")
SUBMODULE = Path("vendor/ableton-link")
ROOT = Path(".")

# Must stay in step with the schema block in link-upstream-watch.md and with the
# `risk: api-break` branch in link-upstream-port.md. Omitting a risk the watch
# agent is told to emit produces an item that can never pass the required check.
RISKS = {"behavior", "wire-format", "api-break", "internal"}
STATUSES = {"outstanding", "retired"}
SHA_RE = re.compile(r"^[0-9a-f]{7,40}$")
ID_RE = re.compile(r"^[a-z0-9][a-z0-9-]*$")

errors, warnings = [], []


def err(msg):
    errors.append(msg)


def warn(msg):
    warnings.append(msg)


def short(s):
    return s[:7]


# --- parse ------------------------------------------------------------------
if not BACKLOG.exists():
    print(f"error: {BACKLOG} does not exist")
    sys.exit(1)

try:
    doc = tomllib.loads(BACKLOG.read_text())
except tomllib.TOMLDecodeError as e:
    print(f"error: {BACKLOG} is not valid TOML: {e}")
    sys.exit(1)

if doc.get("schema_version") != 1:
    err(f"schema_version must be 1, got {doc.get('schema_version')!r}")

port = doc.get("port", [])
undecided = doc.get("undecided", [])
not_applicable = doc.get("not_applicable", [])

if not port:
    err("no [[port]] items; an empty backlog should still list its buckets")

# --- structure --------------------------------------------------------------
REQUIRED = ["id", "title", "upstream", "rust", "risk", "status",
            "retired_at_pin", "why"]

seen_ids, sha_owner = {}, {}

for n, item in enumerate(port):
    where = f"[[port]] #{n + 1} ({item.get('id', '<no id>')})"

    for key in REQUIRED:
        if key not in item:
            err(f"{where}: missing required key `{key}`")
    if any(k not in item for k in REQUIRED):
        continue

    if not ID_RE.match(item["id"]):
        err(f"{where}: id must be a lowercase slug, got {item['id']!r}")
    if item["id"] in seen_ids:
        err(f"{where}: duplicate id, already used by [[port]] #{seen_ids[item['id']] + 1}")
    seen_ids[item["id"]] = n

    if not item["title"].strip():
        err(f"{where}: title is empty")
    if not item["why"].strip():
        err(f"{where}: why is empty — every item has to say why it matters")

    if item["risk"] not in RISKS:
        err(f"{where}: risk must be one of {sorted(RISKS)}, got {item['risk']!r}")
    if item["status"] not in STATUSES:
        err(f"{where}: status must be one of {sorted(STATUSES)}, got {item['status']!r}")

    # There is deliberately no `issue` field. A tracking-issue number stored
    # here is a second copy of state that nothing keeps in sync, which is the
    # same defect as the checkboxes this file replaced. The issue is found by
    # its `upstream-backlog-id` marker instead, so `id` stays the only key.
    if "issue" in item:
        err(f"{where}: remove the `issue` key — the tracking issue is located "
            "by its `upstream-backlog-id` marker, not recorded here")

    if not item["upstream"]:
        err(f"{where}: no upstream SHAs — an item with no commit cannot be ported")
    for sha in item["upstream"]:
        if not SHA_RE.match(sha):
            err(f"{where}: {sha!r} is not a lowercase hex SHA of 7-40 chars")
            continue
        # Within [[port]] a SHA must be owned by exactly one item. Two items
        # claiming one commit is unresolvable: the watermark passes it once, so
        # whichever ports first strands the other pointing at a retired SHA.
        if short(sha) in sha_owner:
            err(f"{where}: upstream {sha} is also claimed by "
                f"[[port]] `{sha_owner[short(sha)]}` — one commit, one item")
        else:
            sha_owner[short(sha)] = item["id"]

    # Retirement has to be auditable: a retired item records the pin it was
    # retired at, and an outstanding one must not pretend it was.
    pin = item["retired_at_pin"]
    if item["status"] == "retired" and not pin:
        err(f"{where}: status is retired but retired_at_pin is empty")
    if item["status"] == "outstanding" and pin:
        err(f"{where}: status is outstanding but retired_at_pin is set to {pin!r}")
    if pin and not SHA_RE.match(pin):
        err(f"{where}: retired_at_pin {pin!r} is not a hex SHA")

    # `blocked_on` marks an item that cannot be ported mechanically because it
    # needs a design decision first - not one that is merely risky. The port
    # agent skips such an item and moves to the next, exactly as it does for
    # `risk = "api-break"`, so that one undecidable item cannot head-of-line
    # block every item behind it. The watermark is unaffected: the item is
    # still `outstanding`, so the pin still cannot cross it.
    #
    # It is deliberately free text rather than a flag. The whole point is to
    # state the decision a human owes, and a bare `blocked = true` records that
    # someone gave up without recording what they gave up on.
    #
    # The field is optional, so its absence is valid and dropping one is silent
    # here. `link-upstream-watch.md` rewrites this file wholesale, and is
    # therefore told in its schema block to carry `blocked_on` across verbatim
    # and never to write one - that instruction is what this check cannot do.
    if "blocked_on" in item:
        if item["status"] != "outstanding":
            err(f"{where}: blocked_on is set but status is {item['status']!r} — "
                "only an outstanding item can be blocked")
        if not isinstance(item["blocked_on"], str) or not item["blocked_on"].strip():
            err(f"{where}: blocked_on must be a non-empty string naming the "
                "decision that has to be made before this can be ported")

for n, entry in enumerate(undecided):
    if not entry.get("upstream"):
        err(f"[[undecided]] #{n + 1}: no upstream SHAs")
for n, entry in enumerate(not_applicable):
    if not entry.get("upstream"):
        err(f"[[not_applicable]] #{n + 1}: no upstream SHAs")
    if not entry.get("reason", "").strip():
        err(f"[[not_applicable]] #{n + 1}: no reason given")

wm = doc.get("watermark", {})
for key in ("pinned", "upstream"):
    if not re.fullmatch(r"[0-9a-f]{40}", wm.get(key, "")):
        err(f"[watermark] {key} must be a full 40-character SHA")

# Coverage comes only from the explicit `upstream` arrays. A SHA mentioned in
# prose used to count too, which meant a drift commit could satisfy coverage
# while no bucket owned it: the port workflow would never select it, and the
# pin would eventually cross it unported. Prose is documentation, not a bucket.
covered = set()
for bucket in (port, undecided, not_applicable):
    for entry in bucket:
        covered.update(short(s) for s in entry.get("upstream", []))

# --- watermark agreement ----------------------------------------------------
# This is the check the old checkbox scheme could not make. The issue body could
# claim an item was outstanding long after the pin had moved past it, and the
# only reason that never broke anything is that the port workflow ignored the
# checkboxes entirely and re-derived truth from the pin on every run.
def git(*args):
    return subprocess.run(["git", "-C", str(SUBMODULE), *args],
                          capture_output=True, text=True)


if not (SUBMODULE / ".git").exists():
    warnings.append(f"{SUBMODULE} is not checked out; skipped watermark checks. "
                    "CI checks out submodules recursively and does run them.")
else:
    pinned = git("rev-parse", "HEAD").stdout.strip()

    if wm.get("pinned") and pinned and wm["pinned"] != pinned:
        err(f"[watermark] pinned is {short(wm['pinned'])} but the actual "
            f"vendor/ableton-link pin is {short(pinned)}. The file is stale; "
            "rerun triage or correct the header.")

    ahead = git("log", "--reverse", "--no-merges", "--pretty=format:%H",
                f"{pinned}..FETCH_HEAD")
    if ahead.returncode != 0 or not ahead.stdout.strip():
        ahead = git("log", "--reverse", "--no-merges", "--pretty=format:%H",
                    f"{pinned}..origin/master")

    if ahead.returncode != 0:
        warn("could not compute the drift range (no upstream ref fetched); "
             "skipped coverage and retirement checks")
    else:
        drift = [s for s in ahead.stdout.split("\n") if s.strip()]
        drift_short = {short(s) for s in drift}

        # The file is accountable for the range it was last triaged against:
        # pin..[watermark].upstream. Commits that landed upstream *after* that
        # are not a backlog defect — nobody has had the chance to triage them —
        # and erroring on them would turn every unrelated pull request red the
        # moment Ableton pushes, between the 30-day watch runs.
        #
        # Nothing is lost by warning instead. The pin-advance check below is the
        # one that actually protects work, and it refuses to cross an
        # unclassified commit whether or not it is inside the triaged range.
        wm_upstream = short(wm.get("upstream", ""))
        if wm_upstream in {short(s) for s in drift}:
            cutoff = [short(s) for s in drift].index(wm_upstream) + 1
        else:
            # The header names a commit that is not in the range — either it is
            # the pin itself (nothing triaged yet) or the file is stale in a way
            # the `pinned` check above did not catch. Hold the file to
            # everything; a false error here is louder and safer than silence.
            cutoff = len(drift)
        triaged = [short(s) for s in drift[:cutoff]]
        arrived_since = [short(s) for s in drift[cutoff:]]

        # Coverage: a commit nobody mentions is not deferred, it is deleted the
        # moment the pin moves past it.
        for sha in sorted(set(triaged) - covered):
            err(f"upstream commit {sha} is ahead of the pin but appears nowhere "
                "in the backlog — triage it into a bucket")

        new_untriaged = sorted(set(arrived_since) - covered)
        if new_untriaged:
            warn(f"{len(new_untriaged)} upstream commit(s) landed after "
                 f"[watermark].upstream ({wm_upstream}) and are not triaged yet: "
                 f"{', '.join(new_untriaged[:8])}"
                 f"{' ...' if len(new_untriaged) > 8 else ''}. Run the "
                 "link-upstream-watch workflow to bucket them.")

        # An outstanding item whose commits are all behind the watermark is
        # already done; leaving it outstanding makes the port workflow chase
        # work that cannot be ported.
        for item in port:
            if item.get("status") != "outstanding":
                continue
            live = [s for s in item.get("upstream", []) if short(s) in drift_short]
            if not live:
                err(f"[[port]] `{item.get('id')}` is outstanding but none of its "
                    f"commits are ahead of the pin; it is retired in fact — set "
                    f"status = \"retired\" and retired_at_pin = \"{short(pinned)}\"")

        # The converse is deliberately NOT an error. `retired` means the work was
        # done in Rust, not that the pin is past it. Upstream splits one idea
        # across commits that are far apart — five of the ten outstanding items
        # here are non-contiguous, one spanning positions 0-2 and 75-79 — and the
        # pin can only advance as far as the last *fully handled* commit. So a
        # ported item routinely keeps SHAs ahead of the pin for a while.
        #
        # Requiring otherwise would be worse than useless: such an item could
        # never be retired, so the port workflow would pick it again on every run
        # as the earliest outstanding item and re-port work already on main.
        #
        # Nothing is lost by allowing it, because the pin-advance check below
        # keys on `status` rather than on position: the pin still cannot cross a
        # commit whose item is outstanding.
        for item in port:
            if item.get("status") != "retired":
                continue
            live = [s for s in item.get("upstream", []) if short(s) in drift_short]
            if live:
                warn(f"[[port]] `{item.get('id')}` is retired but "
                     f"{', '.join(short(s) for s in live)} is still ahead of the "
                     "pin — expected for a non-contiguous item, but check the "
                     "port really covered those commits")

        # Ancestry order is load-bearing: the port workflow takes the earliest
        # outstanding item and advances a monotonic watermark.
        rank = {short(s): i for i, s in enumerate(drift)}
        order = [min((rank[short(s)] for s in item["upstream"]
                      if short(s) in rank), default=None)
                 for item in port if item.get("status") == "outstanding"]
        order = [o for o in order if o is not None]
        if order != sorted(order):
            err("outstanding [[port]] items are not in upstream ancestry order; "
                "the port workflow advances a monotonic watermark and will port "
                "out of order")

    # --- the commits this change is about to retire --------------------------
    # Everything above derives its expectations from the *new* pin, which leaves
    # the one hole that actually loses work: a commit that advances the gitlink
    # past unported commits and deletes their entries in the same breath is
    # self-consistent afterwards. The crossed commits are behind the new pin, so
    # they are not in `drift`, so nothing above has an opinion about them, and
    # they are gone from every future drift report as well. That is precisely the
    # failure PR #59 produced by hand: it advanced the pin to upstream position 8
    # with positions 1-7 untouched, silently retiring a crash fix.
    #
    # So compare against the pin as it is on `main`, and demand that every commit
    # the change moves past was explicitly accounted for, and accounted for as
    # *finished* work rather than as an open question.
    def gitlink_at(rev):
        r = subprocess.run(["git", "rev-parse", f"{rev}:vendor/ableton-link"],
                           capture_output=True, text=True, cwd=ROOT)
        return r.stdout.strip() if r.returncode == 0 else ""

    base = ""
    for rev in ("origin/main", "main"):
        mb = subprocess.run(["git", "merge-base", "HEAD", rev],
                            capture_output=True, text=True, cwd=ROOT)
        if mb.returncode == 0 and mb.stdout.strip():
            base = mb.stdout.strip()
            break

    old_pin = gitlink_at(base) if base else ""
    if not old_pin:
        warn("could not read the previous submodule pin (no base revision); "
             "skipped the pin-advance check, which is the one that catches "
             "commits being retired without being ported")
    elif old_pin == pinned:
        pass  # the pin did not move in this change
    else:
        crossed = git("log", "--reverse", "--no-merges", "--pretty=format:%H",
                      f"{old_pin}..{pinned}")
        if crossed.returncode != 0:
            warn(f"could not list {short(old_pin)}..{short(pinned)}; skipped the "
                 "pin-advance check")
        else:
            # Which bucket finally owns each commit. `[[port]]` outranks
            # `[[undecided]]`, which outranks `[[not_applicable]]`, so a commit
            # cannot be hidden by adding a second, weaker classification for it.
            owner = {}
            for entry in not_applicable:
                for s in entry.get("upstream", []):
                    owner.setdefault(short(s), ("not_applicable", entry))
            for entry in undecided:
                for s in entry.get("upstream", []):
                    owner[short(s)] = ("undecided", entry)
            for item in port:
                for s in item.get("upstream", []):
                    owner[short(s)] = ("port", item)

            for sha in [s for s in crossed.stdout.split("\n") if s.strip()]:
                k = short(sha)
                if k not in owner:
                    err(f"this change advances the pin past {k}, which appears "
                        "nowhere in the backlog. Once the pin moves it is gone "
                        "from every future drift report — classify it, or move "
                        "the pin back to the commit before it")
                    continue
                kind, entry = owner[k]
                if kind == "undecided":
                    err(f"this change advances the pin past {k}, which is still "
                        f"in [[undecided]] ({entry.get('id')}). The pin may not "
                        "move past an open question — decide it first")
                elif kind == "port" and entry.get("status") != "retired":
                    err(f"this change advances the pin past {k}, but its item "
                        f"[[port]] `{entry.get('id')}` is still outstanding. "
                        "Either port it and set status = \"retired\", or move "
                        "the pin back to the commit before it")

# --- cross-bucket classification --------------------------------------------
# Not an error: a single upstream commit can legitimately touch both a mapped and
# an unmapped path. But the precedence above is what makes it safe, and a reader
# should be able to see where it applied.
multi = {}
for name, bucket in (("port", port), ("undecided", undecided),
                     ("not_applicable", not_applicable)):
    for entry in bucket:
        for s in entry.get("upstream", []):
            multi.setdefault(short(s), set()).add(name)
for sha, kinds in sorted(multi.items()):
    if len(kinds) > 1:
        warn(f"{sha} is classified in more than one bucket ({', '.join(sorted(kinds))}); "
             "port outranks undecided outranks not_applicable when the pin moves past it")

# --- id identity across revisions -------------------------------------------
# `id` is the join key between this file and its tracking issues: the reconcile
# workflow matches on `<!-- upstream-backlog-id: ... -->`, never on the title.
# So an id is a permanent name, not a label. Renaming or deleting one passes
# every check above — the new document is internally consistent — and then the
# reconcile workflow opens a fresh issue for the new name and fails on the
# orphaned old one *after* the merge, on `main`, where it is expensive to undo.
# Catch it while it is still a pull request by diffing against the merge base.
def backlog_at(rev):
    r = subprocess.run(["git", "show", f"{rev}:{BACKLOG}"],
                       capture_output=True, cwd=ROOT)
    if r.returncode != 0:
        return None
    try:
        return tomllib.loads(r.stdout.decode("utf-8"))
    except (tomllib.TOMLDecodeError, UnicodeDecodeError):
        return None


base_rev = ""
for rev in ("origin/main", "main"):
    mb = subprocess.run(["git", "merge-base", "HEAD", rev],
                        capture_output=True, text=True, cwd=ROOT)
    if mb.returncode == 0 and mb.stdout.strip():
        base_rev = mb.stdout.strip()
        break

old_doc = backlog_at(base_rev) if base_rev else None
if old_doc is None:
    warn("could not read the previous revision of the backlog; skipped the "
         "id-stability check")
else:
    live_ids = {i.get("id") for i in port}
    for old in old_doc.get("port", []):
        oid = old.get("id")
        if oid and oid not in live_ids:
            err(f"[[port]] `{oid}` existed at the merge base and is gone from "
                "this revision. Ids are permanent — the tracking issue for it is "
                "matched by id and would be orphaned. Retire the item "
                '(status = "retired") instead of renaming or deleting it.')


for w in warnings:
    print(f"warning: {w}")
for e in errors:
    print(f"error: {e}")

n_out = sum(1 for i in port if i.get("status") == "outstanding")
print(f"\n{BACKLOG}: {len(port)} port items ({n_out} outstanding), "
      f"{len(undecided)} undecided group(s), {len(not_applicable)} not-applicable "
      f"entries, {len(covered)} distinct commits accounted for")

if errors:
    print(f"FAILED with {len(errors)} error(s)")
    sys.exit(1)
print("OK")

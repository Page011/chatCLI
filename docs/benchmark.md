# Memory benchmark

Low memory use is the reason chatcli exists. `docs/DESIGN.md` says there is no claim without both numbers, so this page fixes the method and holds the results.

**Status: no numbers yet.** The script is written and self-checked on a harmless process (a console and its child). It has not been run against chatcli or a desktop app. The tables below stay empty until someone runs the three sessions. Do not quote a figure from this page until a row is filled in.

## What is measured

Every 5 seconds the script adds up, over **the whole process tree** (the root process and all its descendants):

- **Working set**: memory currently in RAM for the processes, shared pages counted in each process that maps them.
- **Private bytes**: memory the processes committed for themselves. This is the fairer number across apps, because it leaves out shared libraries.

Desktop chat apps start many helper processes (renderers, GPU, utility). Measuring only the parent is the usual mistake, and it makes an app look far smaller than it is. The script follows parent ids, so helpers are included. The largest process count seen is printed so you can see it worked.

**Not measured:** CPU, battery, network, disk, start-up time, and any feature comparison. A small memory figure says nothing about those.

## The fixed session

Same on every run, every app, in this order. Phase names are in the CSV.

| Phase | Name in CSV | Length | What happens |
|---|---|---|---|
| 1 | `idle-start` | 60 s | Cold start, already signed in, nothing touched |
| 2 | `open-and-scroll` | 5 min | Open 10 chats, scroll 200 messages in each |
| 3 | `receive` | 10 min | Messages arrive (see below) |
| 4 | `idle-end` | 5 min | Nothing touched |

Phase 2 is driven by hand and takes as long as it takes: the 5 minutes is a fixed window. Do the same scrolling every run. If you finish early, wait; do not use the spare time for anything else.

Phase 3 for each app:

- **chatcli `--demo`**: the demo backend produces the incoming messages. This skips the network and TLS cost, so it is a lower bound.
- **chatcli live**: a second test account sends one short message a minute for 10 minutes. Label this run `chatcli-live`, never mix it with the demo rows.
- **Desktop apps**: the same test account pair, the same one message a minute.

Use test accounts only. Never run a session on a real work account, and never put real names, numbers or chat content in a result you publish. In this repo use placeholders (Alex Example, Sam Sample).

## Running it

Windows PowerShell 5.1 or PowerShell 7. From the repository root, in a new window each run (close other heavy apps first, and use the same machine, power mode and window size every time):

```
powershell -ExecutionPolicy Bypass -File scripts\memory-bench.ps1 -Label chatcli-demo -Launch .\chatcli.exe -LaunchArgs '--demo' -AppVersion 0.2.0 -BuildProfile release
```

For an app you start and sign in to yourself, attach by process name (no `.exe`; every process with that name is a root):

```
powershell -ExecutionPolicy Bypass -File scripts\memory-bench.ps1 -Label desktop-app -ProcessName ms-teams -AppVersion "version from the About page" -BuildProfile n/a
```

Start the script, then start the session within the first few seconds. The script prints `PHASE: <name>` when each phase begins so you know what to do. It writes `bench-out\<label>-<time>.csv` (one row per sample) and `.meta.txt` (OS version and build, app version, build profile, phases, interval). `-Summarize <csv>` prints the summary again.

The script only watches. It never signs in, sends or types. A process it launched is stopped by id with its children at the end; an app you attached to is left running.

Run **three times** per app. Keep all three CSVs.

## Results

Fill one row per run. All figures MB, from the script's summary, summed over the process tree. Spread = highest peak minus lowest peak of the three runs, as a share of the median peak.

Machine: _(CPU, RAM, Windows version and build: copy from the `.meta.txt`)_

### Private bytes

| App | Version | Run | Min | Median | Peak | End |
|---|---|---|---|---|---|---|
| chatcli `--demo` | | 1 | | | | |
| | | 2 | | | | |
| | | 3 | | | | |
| chatcli live | | 1 | | | | |
| | | 2 | | | | |
| | | 3 | | | | |
| Desktop app A | | 1 | | | | |
| | | 2 | | | | |
| | | 3 | | | | |

Spread of peaks: _not measured_

### Working set

Same table, same rows.

## Reading the numbers

- A rising **End** well above the phase 1 value after a quiet phase 4 is a leak or an unbounded cache. Report it as a finding, do not average it away.
- Three runs far apart (spread over about 15 percent) mean the session was not repeatable: fix the session before you trust any figure.
- The demo rows are not comparable with live rows. Compare like with like only.

## Check before publishing a number

- [ ] Three runs, all CSVs kept
- [ ] The largest process count of each desktop app is more than 1
- [ ] `.meta.txt` files show the same machine, OS build and build profile
- [ ] No account names, numbers or chat text in anything published
- [ ] The README links to this page next to every memory figure

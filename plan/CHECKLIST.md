# The live walk-through

Everything in the plan's "Done when" lists is checked by the test suite, the
gate, or a pty run against a hand-written cache — none of it needs Azure.
This file is the other half: the things only a real subscription can answer.

az-tui was built without one. The endpoints were verified against the
published Azure REST reference rather than against a live tenant, and the
places where the reference disagreed with the plan are in the commit messages
(`git log --grep 'REST reference'`). Until this list is ticked, treat the
first run against a real subscription as the real acceptance test.

Tick these in order, on a machine that can reach the subscription.

1. `az login`; `az account list -o table` shows the subscriptions you expect.
2. `az-tui doctor`:
   - the `az`, account and subscription lines answer in under a second;
   - both token lines are `ok` and under a second each;
   - every vault and registry named in `config.toml` is found, and any that
     is not is listed under `not found`;
   - a count per vault and per registry, and the whole thing exits 0.
3. `az-tui --no-cache`: the first frame inside a second, the vaults landing
   one by one, and when it settles the status bar reads
   `● N vaults · M secrets · just now`.
4. Quit and start again without `--no-cache`: the same rows are on the first
   frame, and the status bar says how old they are.
5. `/` a secret name that exists in two vaults: both rows, adjacent, in the
   configuration's vault order rather than the alphabet.
6. `v` on a secret you may read: it is shown, it counts down, and it is gone
   at zero. Then `y`, and paste it somewhere: byte-identical. Check a value
   with a trailing newline and one with `=` padding characters — both are
   named in the clipboard tests, but only a real paste proves the terminal
   did not eat one.
7. `v` on a secret you may **not** read: the refusal appears under Value and
   the table is otherwise untouched.
8. `grep -c '<the value you revealed>' ~/.local/share/az-tui/*` is 0 for both
   files. (On macOS, `~/Library/Application Support/az-tui/`.)
9. `2`, find a repository, `Enter`, `y`, then `docker pull <paste>` succeeds.
10. `Y` at the tag level, then `docker pull <paste>` or
    `crane manifest <paste>`: the digest reference is accepted too.
11. Over SSH into that machine, from a terminal that speaks OSC 52 (kitty,
    WezTerm, Alacritty, foot, iTerm2, Windows Terminal, VS Code's): `y` still
    lands the value on the **local** clipboard. Inside tmux with
    `set -s set-clipboard on`: the same.
12. Wrong on purpose:
    - `--subscription 00000000-0000-0000-0000-000000000000` says so in the
      status bar and still shows the cache;
    - a vault name in `config.toml` that does not exist is listed by `doctor`
      under `not found`;
    - a registry the login has no role on says `no permission (needs AcrPull…)`
      and the other registries still fill in.
13. Throttle: hold `r`. When Azure answers 429 the status bar says how long it
    was asked to wait, and the app stays responsive throughout.

## Things the documentation left open

Worth confirming on the first live run, because no doc settles them:

- **Key Vault `api-version=7.4`.** It is no longer in the spec repository but
  no retirement has been announced. If a vault refuses it, `API_VERSION` in
  `src/azure/vault.rs` is the one constant to change; `2025-07-01` reads the
  same four shapes.
- **The registry exchange's `tenant` field.** The swagger calls it optional
  and the prose calls it required. az-tui sends it when `az` will say what it
  is. If a registry refuses the exchange with the tenant present, that is
  worth knowing.
- **ABAC registries.** Under the newer role-assignment mode `AcrPull` does not
  grant catalog listing. If the Registries tab lists no repositories but tags
  read fine once a repository is named, the login wants
  `Container Registry Repository Catalog Lister`.

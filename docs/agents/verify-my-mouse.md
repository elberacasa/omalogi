# Prompt: verify my mouse

For owners of a mouse Omalogi lists as untested. It takes a few minutes, runs only on a
spare profile slot, restores everything at the end, and makes the model verified for
everyone.

You do not need a checkout or an agent. With the helper installed:

1. `omalogi check` reads every profile and writes nothing. Every check should pass.
2. `omalogi verify` runs the self-test. It says what it will do and asks before it starts,
   backs up all profile memory, runs on the first profile that is turned off, and puts
   every byte back at the end.
3. It ends with a link to the Mouse report form, prefilled with the model and the result.
   Open it and send it. A maintainer then marks the model verified.

The overlay's **Help verify it** runs the same self-test in a terminal.

The rest of this guide is for a coding agent that also opens the pull request. The
quickest start is one sentence to it:

> Clone https://github.com/elberacasa/omalogi and follow docs/agents/verify-my-mouse.md
> to verify my mouse.

## Claim the model first

Each model has one issue, listed in [#15](https://github.com/elberacasa/omalogi/issues/15).
The issue's assignee is the person verifying it, so two owners never test the same model.

Agents: run step 1 below first and take the model, its connection, product id and active
firmware from `omalogi --json info` instead of asking the owner to fill in the
placeholders. Then search for the model's issue with
`gh issue list --label verify-a-mouse --state open --search <product id>`, or on
https://github.com/elberacasa/omalogi/issues?q=label%3Averify-a-mouse+is%3Aopen if `gh`
is not set up.

- **The model has an issue with no assignee:** the owner comments `/claim` there and is
  assigned. `/unclaim` gives it back.
- **The model has an issue with an assignee:** someone is already on it. Stop, unless the
  claim has had no update for 30 days; then the owner asks in that issue.
- **The model has no issue:** give the owner this link, with the values filled in and
  URL-encoded, to open the Mouse report form and choose "Verify it". The model is
  assigned to them as soon as the issue is opened.
  `https://github.com/elberacasa/omalogi/issues/new?template=mouse.yml&title=[Mouse]:+<model>&model=Logitech+<model>&device_id=046d:<product id>&firmware=<firmware>`

Never open or comment on issues for the owner without asking; it posts under their account.
Keep the issue number for the pull request.

## The verification

```text
Read docs/agents/agent-guide.md, CONTRIBUTING.md and docs/hardware-tests.md first and
follow their hard rules.

I want to verify my Logitech <model> (<wired over USB / through its receiver>) with Omalogi.
Guide me one step at a time and wait for my output after each step:

1. `omalogi --version` and `omalogi --json info`: confirm the mouse is found, and note its
   name, USB id or WPID, and firmware. Never ask for or record its unit ID.
2. `omalogi check`: it reads every profile and writes nothing. Every check should pass.
   If one fails, stop here: help me post its output on the model's issue, and do not
   edit the mouse.
3. `omalogi verify`, and read me its summary. It asks before it starts; I answer. It
   backs up all profile memory, uses the first profile that is turned off (or
   `--profile N` if every profile is on), writes only that profile and the profile
   directory, and puts every byte back at the end, then prints a prefilled report link.
4. If anything failed: profile memory was already put back from the backup it names.
   Help me post its summary on the model's issue, or send the link it printed, instead
   of opening a pull request.
5. If every check passed, open a pull request against the `next` branch that:
   - sets `verified` for this exact model and connection in src/hidraw.rs
     (SUPPORTED_DEVICES for USB, WIRELESS_DEVICES for a receiver), and adds its profile
     layout to VERIFIED_LAYOUTS in src/onboard/format.rs if it is not there yet, citing
     `omalogi verify` as the evidence;
   - adds an entry to docs/hardware-tests.md with the date, model, USB id or WPID,
     firmware, profile layout, the `omalogi check` result and the `omalogi verify`
     summary (no unit ID, no backups);
   - moves the model to the verified list in the README;
   - says `Closes #<issue>` for the model's issue;
   - passes every check in docs/agents/agent-guide.md.
```

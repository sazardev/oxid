# Real `oxid up` against a live local daemon — DESIGN.md §1/§2 palette
# (Carbon Black / Oxid Orange / Fira Code).
#
# The daemon resolves a project's repo by the literal filesystem path `oxid
# up` runs from, so the demo repo MUST be bind-mounted at the *same absolute
# path* inside the container as on the host (an identity mount) — not
# remapped to something like /demo, or the daemon (running on the host) 404s
# looking for a path that only exists inside the container. Render:
#
#   docker run --rm --network host \
#     -v "$(pwd)":/vhs -w /vhs \
#     -v <oxid-cli-binary>:/usr/local/bin/oxid:ro \
#     -v <scratch-dir-holding-both-demo-repo-and-its-bare-remote>:<same-path> \
#     -e DEMO_REPO=<path-to-the-demo-repo-checkout> \
#     --entrypoint sh ghcr.io/charmbracelet/vhs \
#     -c 'apt-get update -qq && apt-get install -y -qq git \
#         && git config --global --add safe.directory "*" \
#         && vhs oxid-deploy-demo.tape'
#
# (git isn't in the base VHS image, hence the apt-get install; the
# safe.directory wildcard is needed because the repo/remote are owned by the
# host UID, not the container's root — without it `git push` fails with
# "detected dubious ownership". The bare `origin` remote must be mounted too,
# at its own real path, or `git push` fails with "does not appear to be a
# git repository".)

Output oxid-deploy-demo.gif
Output oxid-deploy-demo.mp4

Set Shell "bash"
Set FontFamily "Fira Code"
Set FontSize 24
Set Width 1200
Set Height 630
Set Padding 32
Set WindowBar Rings
Set WindowBarSize 42
Set BorderRadius 0
Set TypingSpeed 45ms
Set Theme {"name":"Oxid","black":"#121212","red":"#DE5236","green":"#4A9E79","yellow":"#DE5236","blue":"#6B7280","purple":"#DE5236","cyan":"#4A9E79","white":"#F4F4F5","brightBlack":"#333333","brightRed":"#DE5236","brightGreen":"#4A9E79","brightYellow":"#DE5236","brightBlue":"#6B7280","brightPurple":"#DE5236","brightCyan":"#4A9E79","brightWhite":"#F4F4F5","background":"#121212","foreground":"#F4F4F5","cursor":"#DE5236","selection":"#333333"}

Type "cd $DEMO_REPO"
Enter
Sleep 400ms

Type "export OXID_API=http://127.0.0.1:8080 OXID_TOKEN=demo-marketing-token-please-change"
Enter
Sleep 600ms

Type "git push origin feature-login"
Enter
Sleep 1200ms

Type "oxid up feature-login"
Enter
Sleep 3500ms

Type "oxid status"
Enter
Sleep 2500ms

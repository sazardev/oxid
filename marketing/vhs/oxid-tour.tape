# Quick CLI tour against a live local daemon — same brand theme and same
# identity-mount requirement as oxid-deploy-demo.tape; see that file's header
# for the full render command.

Output oxid-tour.gif
Output oxid-tour.mp4

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

Type "oxid ps"
Enter
Sleep 2000ms

Type "oxid status"
Enter
Sleep 2000ms

Type "oxid logs feature-login | tail -6"
Enter
Sleep 2500ms

# Contributing

## GUI release checklist

Run this once, in full, before releasing the GNOME GUI on real hardware.

- [ ] Pair the Brother MFC through `scanbus-gui` and confirm it appears as paired after restarting the GUI.
- [ ] Assign a profile to one hardware key and confirm the buttons page still shows that assignment after restarting the daemon.
- [ ] Press the assigned key on the scanner and confirm a notification appears without opening the GUI window first.
- [ ] Click `Open` from the success notification and confirm the scanned file opens.
- [ ] Repeat with the scanner offline and confirm the GUI reports the failure without stacking multiple notifications.
- [ ] Repeat with the daemon restarted after the job begins and confirm the GUI recovers without leaving stale progress notifications behind.
- [ ] Record the date, scanner model, and result in the release notes or the tracking issue before shipping.

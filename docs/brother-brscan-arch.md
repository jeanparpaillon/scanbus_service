Brother’s **brscan** is not a single standardized protocol. It is Brother’s proprietary network-scanning stack, used by the `brscan2/3/4/5` SANE backends and by `brscan-skey`. For your scanbus use case, the particularly interesting part is **`brscan-skey`**, because it implements the *scanner-initiated “Scan to PC”* workflow. ([Brother Support][1])

The architecture is roughly:

```text
                    Brother MFP
                ┌──────────────────┐
                │ scanner firmware │
                └───────┬──────────┘
                        │
       registration     │ SNMP / UDP 161
       PC → printer     │
                        │
                ┌───────▼──────────┐
                │ registered PC:   │
                │ "desktop"        │
                │ 192.168.1.20     │
                │ UDP 54925        │
                └───────┬──────────┘
                        │
      user presses      │
      "Scan → PC"       │
                        │ UDP 54925
                        ▼
                ┌──────────────────┐
                │  brscan-skey     │
                │  on PC           │
                └───────┬──────────┘
                        │
                        │ invoke scan
                        ▼
                ┌──────────────────┐
                │ Brother SANE     │
                │ brscan backend   │
                └──────────────────┘
```

The important distinction is that **54925 is primarily the Scan-to-PC notification/control path; it is not simply a port over which the printer streams the scanned image.**

### 1. PC registration on the MFP

`brscan-skey` first registers the computer with the printer. Reverse-engineered captures show that this is done using **SNMP SET requests to UDP/161**, using Brother's private enterprise OIDs (`1.3.6.1.4.1.2435...`). A real capture exposes registration strings such as:

```text
TYPE=BR;
BUTTON=SCAN;
USER="root";
FUNC=IMAGE;
HOST=192.168.1.20:54925;
APPNUM=1;
DURATION=360;
BRID=;
```

and analogous registrations for:

```text
FUNC=IMAGE   APPNUM=1
FUNC=OCR     APPNUM=3
FUNC=EMAIL   APPNUM=2
FUNC=FILE    APPNUM=5
```

This is particularly useful because it reveals the model: the PC tells the MFP **“I am a Scan-to-PC destination, display me under these functions, and contact me at IP:54925.”** ([Arch User Repository][2])

`DURATION=360` also indicates that registration has a lifetime rather than necessarily being permanent; implementations therefore refresh their registration.

Brother's official `brscan-skey` interface confirms the user-facing pieces of this mechanism: a PC can advertise a name of up to 15 alphanumeric characters and optionally a four-digit password. ([Brother Support][1])

### 2. The MFP discovers the registered PC

After registration, the Brother LCD can expose something along the lines of:

```text
Scan
 └─ to PC
     ├─ Image
     │    └─ desktop
     ├─ OCR
     │    └─ desktop
     ├─ Email
     │    └─ desktop
     └─ File
          └─ desktop
```

This explains why Brother's mechanism is different from generic eSCL/AirScan discovery.

The destination PC is effectively **registered into the scanner**, rather than the scanner merely discovering arbitrary `_scanner._tcp` clients.

### 3. Scanner → PC notification

When the user selects the PC and presses Scan, the MFP contacts the registered host on **UDP port 54925**.

Brother officially documents UDP/54925 as the firewall port required for network scanning, including scanner-panel-initiated scanning. ([soutien.brother.ca][3])

Conceptually:

```text
MFP                         PC
 |                           |
 | SNMP SET registration     |
 |<--------------------------|
 |       UDP/161             |
 |                           |
 | user selects "desktop"    |
 |                           |
 | Scan request              |
 |-------------------------->|
 |       UDP/54925           |
```

The exact payload/state machine varies across Brother generations. That's one reason I would avoid treating “brscan protocol” as one stable wire specification. Recent reverse-engineering work on newer devices also reports several exchanges/ports before the actual scan operation begins. ([Reddit][4])

### 4. `brscan-skey` does not normally receive the image itself

This is the architectural detail most relevant to scanbus.

Once `brscan-skey` receives the button event, it launches a configured handler. Brother documents handlers such as:

```text
IMAGE=".../scantoimage-....sh"
OCR=...
EMAIL=...
FILE=".../scantofile-....sh"
```

([Brother Support][1])

Those scripts are passed the scanner device and then normally invoke the **Brother SANE backend** to perform the actual scan. For example, a configured scanner may appear as:

```text
brother4:net1;dev0
```

and scanning can ultimately look like:

```bash
scanimage --device 'brother4:net1;dev0'
```

([Gist][5])

So there are really two protocols/workflows:

```text
          SCAN-TO-PC CONTROL

 MFP ------------------------> brscan-skey
             UDP 54925
              event

                     │
                     │ launch handler
                     ▼

          ACTUAL SCAN ACQUISITION

 scanimage / SANE -----------> MFP
       Brother brscan backend
                │
                │ image data
                ▼
              file
```

This is surprisingly elegant: pressing Scan on the printer doesn't necessarily mean **“printer pushes image to PC.”** It means approximately **“printer asks PC to initiate a scan against me.”**

### 5. There are multiple Brother generations

You will encounter:

```text
brscan
brscan2
brscan3
brscan4
brscan5
brscan-skey
```

`brscan2/3/4/5` are generations of Brother's scanner/SANE support. `brscan-skey` is the complementary daemon implementing scanner-panel Scan-to-PC integration.

There are also newer Brother products where Brother documents **TCP 5566** for network scanning and **TCP 54921** specifically for Brother iPrint&Scan, so don't assume every modern Brother device uses exactly the older brscan4 wire behavior. ([Brother Support][6])

### Implication for scanbus

For scanbus, I would model Brother Scan-to-PC as two independent capabilities:

```text
Brother backend
│
├── scan acquisition
│     scanner → SANE/brscan
│
└── push-button service
      │
      ├── register PC
      │      SNMP SET / UDP 161
      │
      ├── listen
      │      UDP 54925
      │
      └── translate event
             ↓
          scanbus API
             ↓
          start scan
```

In other words, **you probably don't need to reproduce the entire Brother scanning protocol to support the Brother “Scan to PC” button**. You could implement just the `brscan-skey` registration/event protocol and let an existing acquisition backend—Brother SANE initially, perhaps eSCL where supported—perform the scan.

There is also useful reverse-engineering work to build on: the older `brother2/PROTO` work referenced by the `brother-scan` project, and a new 2026 Python reimplementation of `brscan-skey` targeting newer Brother models. ([GitLab][7])

If your goal is implementing a **Brother backend for scanbus**, the next useful step would be to reconstruct the actual **SNMP registration + UDP/54925 message formats and state machine**, byte-for-byte, and identify which portions differ between brscan3/4/5. That would give you a fairly small protocol implementation rather than depending on Brother's binary `brscan-skey`.

[1]: https://origin.supportbrothercom.brother.co.jp/g/b/faqend.aspx?c=us_ot&faqid=faq00100714_000&lang=en&pfs=1&prod=hll2480dw_us_as&utm_source=chatgpt.com "Configure and use the Scan Key Tool (Linux) | Brother"
[2]: https://aur.archlinux.org/packages/brscan-skey?O=0&all_reqs=1&utm_source=chatgpt.com "AUR (en) - brscan-skey"
[3]: https://soutien.brother.ca/app/answers/detail/a_id/83780/~/quels-ports-du-pare-feu-dois-je-ouvrir-pour-permettre-au-r%C3%A9seau-de-communiquer?utm_source=chatgpt.com "Quels ports du pare-feu dois-je ouvrir pour permettre au réseau de communiquer avec l'appareil Brother? - Brother Canada"
[4]: https://www.reddit.com/r/printers/comments/1s2k622/brother_printer_scanner_driver_brscanskey_in/?utm_source=chatgpt.com "Brother printer scanner driver \"brscan-skey\" in python for raspberry or similar"
[5]: https://gist.github.com/ianhattendorf/f85a36fac520a1346b82716460f84001?permalink_comment_id=5082306&utm_source=chatgpt.com "Minimal instructions to install Printer/Scanner combo \"Brother DCP-J4120DW\" under Arch Linux · GitHub"
[6]: https://support.brother.com/g/b/faqend.aspx?c=gb&faqid=faq00100466_503&lang=en&prod=ads4500w_eu&utm_source=chatgpt.com "Your Brother Machine Cannot Scan over the Network | Brother"
[7]: https://gitlab.com/esben/brother-scan/-/blob/19fdbd1de7d16908623fb0b1811370d63a5a3265/README.md?utm_source=chatgpt.com "README.md · 19fdbd1de7d16908623fb0b1811370d63a5a3265 · Esben Haabendal / brother-scan · GitLab"

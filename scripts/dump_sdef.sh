#!/bin/bash
# Dumps the Mail.sdef four-char codes bite's Apple Event builders rely on.
# CI diffs this against scripts/sdef-codes.pin so a macOS update that changes
# codes fails CI instead of production. Regenerate: ./scripts/dump_sdef.sh > scripts/sdef-codes.pin
set -e
sdef /System/Applications/Mail.app 2>/dev/null | python3 - <<'PYEOF'
import re, sys
s = sys.stdin.read()
want_props = {'read status':'isrd','flagged status':'isfl','junk mail status':'isjk',
  'date sent':'drcv','date received':'rdrc','content':'ctnt','sender':'sndr',
  'subject':'subj','id':'ID  ','mailbox':'mbxp','unread count':'mbuc'}
ok = True
for m in re.finditer(r'<property name="([^"]*)" code="([^"]*)"', s):
    name, code = m.group(1), m.group(2)
    for want, expected in want_props.items():
        if name.lower() == want.lower():
            mark = "OK " if code == expected else "DRIFT"
            if code != expected: ok = False
            print(f"{mark} property {name} = {code} (expected {expected})")
for cls, expected in [('message','mssg'),('mailbox','mbxp'),('outgoing message','bcke'),('attachment','atts')]:
    m = re.search(rf'<class name="({cls})" code="([^"]*)"', s)
    if m:
        mark = "OK " if m.group(2) == expected else "DRIFT"
        if m.group(2) != expected: ok = False
        print(f"{mark} class {m.group(1)} = {m.group(2)} (expected {expected})")
for cmd, expected in [('move','coremove'),('delete','coredelo'),('send','emsgsend')]:
    m = re.search(rf'<command name="{cmd}" code="([^"]*)"', s)
    if m:
        mark = "OK " if m.group(1) == expected else "DRIFT"
        if m.group(1) != expected: ok = False
        print(f"{mark} command {cmd} = {m.group(1)} (expected {expected})")
sys.exit(0 if ok else 1)
PYEOF

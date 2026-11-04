#!/usr/bin/env python3
"""Start the bounded observation service after deployment/recovery tests."""
import json
from deploy import ssh,put,ROOT,LOCAL
facts=json.loads((LOCAL/'deployment.json').read_text())['hosts']
fact=facts['tokyo-ls'];base=fact['base'];directory=fact['home']+'/.config/systemd/user'
put('tokyo-ls',base+'/soak.py',(ROOT/'hosted/soak.py').read_bytes(),0o700)
put('tokyo-ls',directory+'/light-dex-soak.service',f'''[Unit]
Description=Light DEX 24-hour observation
StartLimitIntervalSec=0
[Service]
Type=simple
UMask=0077
MemoryMax=64M
ExecStart=/usr/bin/python3 {base}/soak.py
Restart=on-failure
RestartSec=30
[Install]
WantedBy=default.target
''')
ssh('tokyo-ls','systemctl --user daemon-reload; systemctl --user enable --now light-dex-soak')
# Network services must keep retrying if the private interface is late at boot.
for host,fact in facts.items():
    for node in fact['ids']:
        path=fact['home']+f'/.config/systemd/user/light-dex-validator-{node}.service'
        text=ssh(host,'cat '+path).replace('StartLimitIntervalSec=120\nStartLimitBurst=12','StartLimitIntervalSec=0')
        put(host,path,text+'\n')
    ssh(host,'systemctl --user daemon-reload')
print('24-hour observation started. Results remain pending until the full period ends.')

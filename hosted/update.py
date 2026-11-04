#!/usr/bin/env python3
"""Atomic artifact update with deliberate sequential validator restarts."""
import io,json,pathlib,tarfile,time
from deploy import ssh,put,ROOT,LOCAL
facts=json.loads((LOCAL/'deployment.json').read_text())['hosts']
for host,fact in facts.items():
    base=fact['base'];buffer=io.BytesIO()
    with tarfile.open(fileobj=buffer,mode='w:gz') as tar:
        for name in ['light_dex','light-dex-hosted']:
            p=ROOT/'target/release'/name;info=tar.gettarinfo(str(p),arcname=name);info.mode=0o700
            with p.open('rb') as f:tar.addfile(info,f)
    ssh(host,f'install -d -m 700 {base}/update; tar xzf - -C {base}/update; for f in {base}/update/*; do mv -f "$f" {base}/bin/; done',buffer.getvalue())
    for node in fact['ids']:
        path=fact['home']+f'/.config/systemd/user/light-dex-validator-{node}.service'
        content=ssh(host,'cat '+path).replace('--timeout-ms 8000','--timeout-ms 3000').replace('StartLimitIntervalSec=120\nStartLimitBurst=12','StartLimitIntervalSec=0')
        if '--primary-probe-timeout-ms' not in content:content=content.replace('--timeout-ms 3000','--timeout-ms 3000 --primary-probe-timeout-ms 1500')
        put(host,path,content+'\n')
    if host=='tokyo-ls':
        for p in (ROOT/'hosted/public').iterdir():
            if p.is_file():put(host,base+'/public/'+p.name,p.read_bytes())
    envfile=base+'/runtime.env'
    environment=ssh(host,'cat '+envfile)
    if 'LIGHT_DEX_QUIET=' not in environment:put(host,envfile,environment+'\nLIGHT_DEX_QUIET=1\n')
    ssh(host,'systemctl --user daemon-reload')
for host,fact in facts.items():
    for node in fact['ids']:
        ssh(host,f'systemctl --user restart light-dex-validator-{node}');time.sleep(5)
        status=ssh(host,f'systemctl --user is-active light-dex-validator-{node}')
        if status!='active':raise RuntimeError('Validator failed during rolling update')
        print(f'{host}: validator {node} active')
ssh('tokyo-ls','systemctl --user restart light-dex-gateway light-dex-liquidity')
print('Rolling update finished.')

#!/usr/bin/env python3
"""Configure restricted cross-host data backup using existing operator access."""
import json,os,pathlib,secrets,subprocess
from deploy import ssh,put,LOCAL,ROOT
facts=json.loads((LOCAL/'deployment.json').read_text())['hosts']
for host in facts:
    base=facts[host]['base'];put(host,base+'/backup.py',(ROOT/'hosted/backup.py').read_bytes(),0o700);put(host,base+'/backup-receive.py',(ROOT/'hosted/backup-receive.py').read_bytes(),0o700)
    ssh(host,f'test -f {base}/backup-ssh || ssh-keygen -q -t ed25519 -N "" -f {base}/backup-ssh')
    passfile=LOCAL/('backup-'+host+'.pass')
    if not passfile.exists():passfile.write_text(secrets.token_hex(32));os.chmod(passfile,0o600)
    put(host,base+'/backup.pass',passfile.read_bytes())
for host in facts:
    peer=next(h for h in facts if h!=host);own=facts[host];other=facts[peer];base=own['base'];label='tokyo' if host=='tokyo-ls' else 'ideapad'
    pub=ssh(host,f'cat {base}/backup-ssh.pub')
    line=f'command="{other["base"]}/backup-receive.py {label}",restrict {pub}\n'
    ssh(peer,'install -d -m 700 "$HOME/.ssh"; touch "$HOME/.ssh/authorized_keys"; chmod 600 "$HOME/.ssh/authorized_keys"')
    # Deduplicate the exact restricted key without touching the user's other keys.
    code='import pathlib,sys; p=pathlib.Path.home()/".ssh/authorized_keys"; line=sys.stdin.read(); old=p.read_text(); old=old.replace(chr(92)+"ncommand=",chr(10)+"command="); p.write_text(old if line.strip() in old.splitlines() else old+(chr(10) if old and not old.endswith(chr(10)) else "")+line)'
    import shlex
    ssh(peer,'python3 -c '+shlex.quote(code),line.encode())
    hostkey=ssh(peer,'cat /etc/ssh/ssh_host_ed25519_key.pub')
    put(host,base+'/backup.known_hosts',other['ip']+' '+hostkey+'\n')
    user=pathlib.PurePosixPath(other['home']).name
    put(host,base+'/backup.json',json.dumps({'nodes':own['ids'],'destination':user+'@'+other['ip']}))
    directory=own['home']+'/.config/systemd/user'
    put(host,directory+'/light-dex-backup.service',f'[Unit]\nDescription=Encrypted certified Light DEX data backup\n[Service]\nType=oneshot\nUMask=0077\nMemoryMax=256M\nExecStart=/usr/bin/python3 {base}/backup.py\n')
    put(host,directory+'/light-dex-backup.timer','[Unit]\nDescription=Light DEX backup every six hours\n[Timer]\nOnBootSec=15min\nOnUnitActiveSec=6h\nPersistent=true\n[Install]\nWantedBy=timers.target\n')
    ssh(host,'systemctl --user daemon-reload; systemctl --user enable --now light-dex-backup.timer')
print('Restricted encrypted data backup timers installed; validator keys excluded.')

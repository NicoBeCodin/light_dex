#!/usr/bin/env python3
"""Export certified data, verify replay, encrypt, and copy to the other host.
Consensus signing keys and safety state are deliberately excluded: an old backup
must never silently restore a validator's identity and forget later votes.
"""
import json, os, pathlib, shutil, subprocess, sys, tarfile, tempfile, time
base=pathlib.Path(__file__).resolve().parent
config=json.loads((base/'backup.json').read_text())
with tempfile.TemporaryDirectory(prefix='backup-',dir=base) as directory:
    work=pathlib.Path(directory); (work/'data').mkdir()
    shutil.copyfile(base/'session/manifest.json',work/'manifest.json')
    audits=[]
    for node in config['nodes']:
        for name in [f'node-{node}.json.journal',f'node-{node}.checkpoint',f'node-{node}.checkpoint.previous']:
            source=base/'data'/name
            if source.exists():shutil.copyfile(source,work/'data'/name)
        result=subprocess.run([str(base/'bin/light_dex'),'--audit-journal','--node',str(node),'--chain-config',str(work/'manifest.json'),'--data-dir',str(work/'data')],capture_output=True,text=True,timeout=120)
        if result.returncode:raise RuntimeError('Certified backup replay failed; previous backup preserved')
        audits.append(result.stdout.strip())
    (work/'verification.json').write_text(json.dumps({'created':int(time.time()),'audits':audits,'purpose':'certified exchange data; no validator signing keys or safety rollback'}))
    archive=work/'data.tar.gz'
    with tarfile.open(archive,'w:gz') as tar:
        for path in [work/'data',work/'manifest.json',work/'verification.json']:tar.add(path,arcname=path.name)
    if archive.stat().st_size>256*1024*1024:raise RuntimeError('Backup exceeds transfer budget')
    encrypted=base/'backups/outgoing.gpg'
    encrypted.parent.mkdir(mode=0o700,exist_ok=True)
    subprocess.run(['gpg','--batch','--yes','--pinentry-mode','loopback','--passphrase-file',str(base/'backup.pass'),'--cipher-algo','AES256','--symmetric','--output',str(encrypted),str(archive)],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,timeout=120)
    with encrypted.open('rb') as source:
        subprocess.run(['ssh','-o','BatchMode=yes','-o','ConnectTimeout=15','-o','StrictHostKeyChecking=yes','-o','UserKnownHostsFile='+str(base/'backup.known_hosts'),'-i',str(base/'backup-ssh'),config['destination'],'receive'],stdin=source,check=True,timeout=120)
    print('Verified certified data backup encrypted and copied off-host.')

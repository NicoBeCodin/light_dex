#!/usr/bin/env python3
"""Forced SSH command: accepts one bounded encrypted file, no shell commands."""
import os,pathlib,sys
base=pathlib.Path(__file__).resolve().parent/'backups'
base.mkdir(mode=0o700,exist_ok=True)
name=sys.argv[1]
if name not in ['tokyo','ideapad']:raise SystemExit(2)
target=base/(name+'.latest.gpg');temporary=base/(name+'.incoming')
size=0
with temporary.open('wb') as out:
    os.chmod(temporary,0o600)
    while chunk:=sys.stdin.buffer.read(65536):
        size+=len(chunk)
        if size>256*1024*1024:raise SystemExit('Backup budget exceeded')
        out.write(chunk)
    if size<64:raise SystemExit('Incomplete backup')
    out.flush();os.fsync(out.fileno())
if target.exists():os.replace(target,base/(name+'.previous.gpg'))
os.replace(temporary,target)
fd=os.open(base,os.O_DIRECTORY);os.fsync(fd);os.close(fd)

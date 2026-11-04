#!/usr/bin/env python3
"""Controlled availability checks on this operator's hosted validators."""
import json,pathlib,time,urllib.request
from deploy import ssh,LOCAL
facts=json.loads((LOCAL/'deployment.json').read_text());origin=facts['origin'];hosts=facts['hosts'];checks=[]
def network():
    with urllib.request.urlopen(origin+'/api/v1/network',timeout=12) as response:return json.load(response)
def wait(predicate,seconds=60):
    deadline=time.monotonic()+seconds
    while time.monotonic()<deadline:
        try:
            value=network()
            if predicate(value):return value
        except Exception:pass
        time.sleep(1)
    raise RuntimeError('Live recovery check timed out')
before=wait(lambda n:n.get('status')=='online')
try:
    ssh('tokyo-ls','systemctl --user stop light-dex-validator-0')
    after=wait(lambda n:n.get('height',0)>before['height']+2 and sum(v.get('online',False) for v in n.get('validators',[]))==3)
    checks.append({'test':'single validator stopped','passed':True,'height_before':before['height'],'height_after':after['height']})
finally:ssh('tokyo-ls','systemctl --user start light-dex-validator-0')
wait(lambda n:n.get('status')=='online' and len({v.get('height') for v in n.get('validators',[])})<=2)
# A paused host preserves sockets/state but cannot send votes or answer reads.
try:
    ssh('ideapad-home','systemctl --user kill -s STOP light-dex-validator-2 light-dex-validator-3')
    unavailable=wait(lambda n:n.get('status')=='unavailable')
    time.sleep(4);still=network()
    if still['height']!=unavailable['height']:raise RuntimeError('Unexpected commits without quorum')
    checks.append({'test':'entire remote host paused','passed':True,'status':still['status'],'height':still['height']})
finally:ssh('ideapad-home','systemctl --user kill -s CONT light-dex-validator-2 light-dex-validator-3')
recovered=wait(lambda n:n.get('status')=='online' and n['height']>still['height'])
checks.append({'test':'host resumes without chain reset','passed':True,'height':recovered['height']})
# A complete restart must keep both consensus safety and exchange history.
try:
    for host,fact in hosts.items():ssh(host,'systemctl --user stop '+' '.join('light-dex-validator-'+str(i) for i in fact['ids']))
    audits=[]
    for host,fact in hosts.items():
        base=fact['base']
        for node in fact['ids']:
            output=ssh(host,f'{base}/bin/light_dex --audit-journal --node {node} --chain-config {base}/session/manifest.json --data-dir {base}/data')
            value=json.loads(next(l.removeprefix('JOURNAL_AUDIT ') for l in output.splitlines() if l.startswith('JOURNAL_AUDIT ')))
            audits.append(value)
    # Retained histories can have different starting offsets. Shared digests
    # must appear in the same order; full certificate verification ran in replay.
    anchor=audits[0]['block_digests']
    for value in audits[1:]:
        common=set(anchor)&set(value['block_digests'])
        if not common or [h for h in anchor if h in common]!=[h for h in value['block_digests'] if h in common]:raise RuntimeError('Recovered histories disagree')
    checks.append({'test':'four certified journal replays','passed':True,'validators':4,'retained_blocks':[len(a['block_digests']) for a in audits]})
finally:
    for host,fact in hosts.items():ssh(host,'systemctl --user start '+' '.join('light-dex-validator-'+str(i) for i in fact['ids']))
restarted=wait(lambda n:n.get('status')=='online' and n['height']>recovered['height'])
checks.append({'test':'complete cluster restart','passed':True,'height':restarted['height']})
ssh('tokyo-ls','systemctl --user restart light-dex-gateway light-dex-liquidity')
wait(lambda n:n.get('status')=='online')
smoke=LOCAL/'smoke.json'
if smoke.exists():
    results=json.loads(smoke.read_text())['results']
    for result in results:
        with urllib.request.urlopen(origin+'/api/v1/transactions/'+result['digest'],timeout=10) as response:value=json.load(response)
        if value.get('status')!='committed' or value['receipt'].get('success')!=True:raise RuntimeError('Stored receipt lost after restart')
    checks.append({'test':'gateway restart retains successful visitor receipts','passed':True,'receipts':len(results)})
(LOCAL/'recovery-checks.json').write_text(json.dumps({'at':int(time.time()),'checks':checks},indent=2))
for check in checks:print(json.dumps(check))

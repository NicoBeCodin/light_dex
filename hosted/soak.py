#!/usr/bin/env python3
"""Bounded 24-hour observation. Records failures; never restarts validators."""
import json,os,pathlib,time,urllib.request
base=pathlib.Path(__file__).resolve().parent
startfile=base/'soak-start.json';samples=base/'soak-samples.jsonl';summary=base/'soak-summary.json'
if startfile.exists():state=json.loads(startfile.read_text())
else:
    state={'started_at':int(time.time()),'samples':0,'unavailable_samples':0,'stalled_samples':0,'maximum_replica_lag':0,'first_height':None,'last_height':None,'maximum_rss_kib':0}
    startfile.write_text(json.dumps(state))
def rss_kib():
    total=0
    for p in pathlib.Path('/proc').iterdir():
        if not p.name.isdigit():continue
        try:
            name=(p/'comm').read_text().strip()
            if not name.startswith('light'):continue
            total+=int(next(l for l in (p/'status').read_text().splitlines() if l.startswith('VmRSS:')).split()[1])
        except (OSError,StopIteration,ValueError):pass
    return total
while time.time()-state['started_at']<86400:
    sample={'at':int(time.time()),'host_rss_kib':rss_kib()}
    try:
        with urllib.request.urlopen('http://127.0.0.1:1422/api/v1/network',timeout=5) as response:network=json.load(response)
        sample['network']=network
        heights=[n['height'] for n in network.get('validators',[]) if n.get('online') and isinstance(n.get('height'),int)]
        lag=max(heights)-min(heights) if heights else 0
        state['maximum_replica_lag']=max(state['maximum_replica_lag'],lag)
        if network.get('status') not in ['online','degraded']:state['unavailable_samples']+=1
        height=network.get('height',0)
        if state['first_height'] is None:state['first_height']=height
        if height==state['last_height']:state['stalled_samples']+=1
        state['last_height']=height
    except Exception as error:
        sample['error']=type(error).__name__;state['unavailable_samples']+=1
    state['samples']+=1;state['maximum_rss_kib']=max(state['maximum_rss_kib'],sample['host_rss_kib'])
    usage=os.statvfs(base);sample['free_disk_bytes']=usage.f_bavail*usage.f_frsize
    with samples.open('a') as file:file.write(json.dumps(sample)+'\n')
    state['elapsed_seconds']=int(time.time()-state['started_at']);state['status']='observing'
    temporary=summary.with_suffix('.pending');temporary.write_text(json.dumps(state,indent=2));os.replace(temporary,summary)
    startfile.write_text(json.dumps(state));time.sleep(30)
state['elapsed_seconds']=int(time.time()-state['started_at'])
state['status']='passed' if state['unavailable_samples']==0 and state['stalled_samples']==0 and state['maximum_replica_lag']<=8 and state['samples']>=2800 else 'needs_review'
summary.write_text(json.dumps(state,indent=2))
print('24-hour observation finished: '+state['status'])

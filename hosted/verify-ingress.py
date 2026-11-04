#!/usr/bin/env python3
"""Verify persisted public ingress survives a complete validator outage."""
import json,subprocess,time,urllib.request,urllib.error
from deploy import ssh,LOCAL,ROOT
config=json.loads((LOCAL/'deployment.json').read_text());origin=config['origin'];facts=config['hosts'];base=facts['tokyo-ls']['base']
def get(path):
    with urllib.request.urlopen(origin+'/api/v1/'+path,timeout=15) as response:return json.load(response)
def post(signed):
    request=urllib.request.Request(origin+'/api/v1/transactions',data=json.dumps(signed).encode(),headers={'content-type':'application/json','Origin':origin})
    with urllib.request.urlopen(request,timeout=20) as response:return json.load(response)
def sign(seed,**fields):
    result=subprocess.run(['node',str(ROOT/'hosted/sign-test.mjs')],input=json.dumps({'seed':seed,**fields}),text=True,capture_output=True,check=True)
    return result.stdout
ssh('tokyo-ls','systemctl --user stop light-dex-liquidity')
try:
    seed=ssh('tokyo-ls',f'cat {base}/bot-keys/market-1-role-3.key')
    address=sign(seed,address_only=True)
    account=get('rpc/accounts/'+address);meta=get('meta');market=get('rpc/markets/0')
    price=max(1,int(market['committed_mark_e8']*5//100000000))
    signed=json.loads(sign(seed,chain_id=meta['chain_id'],kind='PlaceOrder',nonce=account['nonce'],fields={'market_id':0,'order_type':'PostOnly','side':'Buy','price':price,'quantity':1}))
    try:
        for host,fact in facts.items():ssh(host,'systemctl --user stop '+' '.join('light-dex-validator-'+str(i) for i in fact['ids']))
        try:post(signed);raise RuntimeError('Unexpected live admission while stopped')
        except urllib.error.HTTPError as error:
            if error.code!=503:raise
        # Signature is public data. Read its reserved digest without resubmitting.
        code='import sqlite3,pathlib; c=sqlite3.connect(pathlib.Path.home()/".local/share/light-dex-public/public.sqlite"); print(c.execute("SELECT digest FROM submissions WHERE json_extract(raw,\'$.signature\')=?",('+repr(signed['signature'])+',)).fetchone()[0])'
        import shlex
        digest=ssh('tokyo-ls','python3 -c '+shlex.quote(code))
    finally:
        for host,fact in facts.items():ssh(host,'systemctl --user start '+' '.join('light-dex-validator-'+str(i) for i in fact['ids']))
    deadline=time.monotonic()+75
    while time.monotonic()<deadline:
        try:
            receipt=get('transactions/'+digest)
            if receipt.get('status')=='committed':break
        except Exception:pass
        time.sleep(1)
    else:raise RuntimeError('Persisted request was not replayed automatically')
    if receipt['receipt'].get('success')!=True:raise RuntimeError('Persisted request failed execution')
    account=get('rpc/accounts/'+address)
    order=next(o for o in account['open_orders'] if o['market_id']==0 and o['price']==price)
    cancel=json.loads(sign(seed,chain_id=meta['chain_id'],kind='CancelOrder',nonce=account['nonce'],fields={'market_id':0,'order_id':order['order_id']}))
    cleanup=post(cancel)
    result={'test':'persisted public request auto-replayed after complete outage','passed':True,'digest':digest,'outcome':receipt['receipt']['success'],'cleanup_admitted':cleanup.get('accepted')}
    (LOCAL/'ingress-check.json').write_text(json.dumps(result,indent=2));print(json.dumps(result))
finally:ssh('tokyo-ls','systemctl --user start light-dex-liquidity')

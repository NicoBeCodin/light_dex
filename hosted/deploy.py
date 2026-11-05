#!/usr/bin/env python3
"""Install a fixed four-validator demo. All host addresses stay in .local/.
Run from a trusted operator computer with the two SSH aliases configured.
Never resets an existing chain; build release binaries before invoking.
"""
import argparse, io, json, os, pathlib, shlex, subprocess, tarfile, tomllib
from frontend import frontend_files
ROOT = pathlib.Path(__file__).resolve().parents[1]
LOCAL = ROOT / 'hosted/.local'
HOSTS = [('tokyo-ls', [0,1], 'Tokyo'), ('ideapad-home', [2,3], 'Home')]
def ssh(host, command, data=None):
    result = subprocess.run(['ssh','-o','BatchMode=yes','-o','ConnectTimeout=15',host,command], input=data, capture_output=True)
    if result.returncode:
        LOCAL.mkdir(parents=True,exist_ok=True,mode=0o700)
        (LOCAL/'last-remote-error.log').write_bytes(result.stderr+result.stdout)
        os.chmod(LOCAL/'last-remote-error.log',0o600)
        raise RuntimeError(f'{host}: remote command failed (details saved privately)')
    return result.stdout.decode().strip()
def put(host, target, content, mode=0o600):
    # Transport through stdin, never put key contents in arguments or output.
    ssh(host, f'install -d -m 700 {shlex.quote(str(pathlib.PurePosixPath(target).parent))}; cat > {shlex.quote(target)}; chmod {mode:o} {shlex.quote(target)}', content.encode() if isinstance(content,str) else content)
def unit(description, base, executable, memory='384M', env=None, inhibit=False):
    command = executable
    if inhibit: command = '/usr/bin/systemd-inhibit --what=sleep:idle:handle-lid-switch --who=LightDEX --why=Validator-host --mode=block ' + command
    return f'''[Unit]
Description={description}
After=network-online.target
StartLimitIntervalSec=0
[Service]
Type=simple
WorkingDirectory={base}
EnvironmentFile={base}/runtime.env
ExecStart={command}
Restart=on-failure
RestartSec=4
TimeoutStopSec=20
UMask=0077
NoNewPrivileges=true
PrivateTmp=true
MemoryMax={memory}
TasksMax=256
LogRateLimitIntervalSec=30
LogRateLimitBurst=100
[Install]
WantedBy=default.target
'''
def main():
    parser=argparse.ArgumentParser();parser.add_argument('--enable-writes',action='store_true');parser.add_argument('--publish',action='store_true');parser.add_argument('--compact-proposals',action='store_true');args=parser.parse_args()
    LOCAL.mkdir(parents=True,exist_ok=True,mode=0o700);os.chmod(LOCAL,0o700)
    facts={}
    for host,ids,region in HOSTS:
        facts[host]={'ip':ssh(host,'tailscale ip -4').splitlines()[0], 'home':ssh(host,'printf %s "$HOME"'), 'ids':ids,'region':region}
    public=ssh('tokyo-ls','curl -fsS --max-time 15 https://api.ipify.org')
    origin='https://light-dex.'+public+'.sslip.io'
    session=LOCAL/'session'
    remote_manifests=[]
    for host,fact in facts.items():
        remote_base=fact['home']+'/.local/share/light-dex-public'
        existing=ssh(host,f'if test -f {remote_base}/session/manifest.json; then cat {remote_base}/session/manifest.json; fi')
        if existing:remote_manifests.append(json.loads(existing))
    if remote_manifests and not (session/'manifest.json').exists():
        raise RuntimeError('A remote chain already exists. Restore the operator session; refusing to replace validator identities.')
    if (session/'manifest.json').exists():
        local_manifest=json.loads((session/'manifest.json').read_text())
        if any(m['chain_id']!=local_manifest['chain_id'] for m in remote_manifests):raise RuntimeError('Remote and local chain identities differ; refusing deployment.')
    if not (session/'manifest.json').exists(): subprocess.run([str(ROOT/'target/release/light-dex-hosted'),'init',str(session)],check=True,stdout=subprocess.DEVNULL)
    peers={str(i):f'{fact["ip"]}:{47000+i}' for fact in facts.values() for i in fact['ids']}
    nodes=[{'id':i,'region':fact['region'],'peer':peers[str(i)],'read':f'http://{("127.0.0.1" if i<2 else fact["ip"])}:{48000+i}'} for fact in facts.values() for i in fact['ids']]
    feeds=[]
    manifest=json.loads((session/'manifest.json').read_text())
    selected=manifest.get('market_ids')
    catalog=tomllib.loads((ROOT/'config/markets.toml').read_text())['markets']
    assets={market['base_asset'] for market in catalog if selected is None or market['id'] in selected}
    for base,symbol in [(1,'BTC'),(2,'ETH'),(4,'BNB'),(5,'XRP'),(3,'SOL')]:
        if base not in assets:continue
        for venue,pair,url in [('binance',symbol+'USDT',f'wss://data-stream.binance.vision/ws/{symbol.lower()}usdt@bookTicker'),('okx',symbol+'-USDT','wss://wseea.okx.com/ws/v5/public'),('coinbase',symbol+'-USD','wss://ws-feed.exchange.coinbase.com')]:
            if symbol=='BNB' and venue=='coinbase':continue
            feeds.append({'base_asset':base,'venue':venue,'symbol':pair,'upstream_url':url})
    for host,ids,region in HOSTS:
        fact=facts[host];base=fact['home']+'/.local/share/light-dex-public';fact['base']=base
        ssh(host,f'install -d -m 700 {base}/bin {base}/session/keys {base}/data {base}/public {fact["home"]}/.config/systemd/user')
        files={
          'bin/light_dex':ROOT/'target/release/light_dex','bin/light-dex-hosted':ROOT/'target/release/light-dex-hosted',
          'bin/light-dex-market-data-hub':ROOT/'market-data-hub/target/release/light-dex-market-data-hub',
          'session/manifest.json':session/'manifest.json','auth.key':session/'auth.key',
        }
        for i in ids:files[f'session/keys/validator-{i}.key']=session/f'keys/validator-{i}.key'
        _, frontend = frontend_files(ROOT/'hosted/public')
        buffer=io.BytesIO()
        with tarfile.open(fileobj=buffer,mode='w:gz') as tar:
            for name,p in files.items():
                info=tar.gettarinfo(str(p),arcname=name);info.mode=0o700 if name.startswith('bin/') else 0o600
                with p.open('rb') as f:tar.addfile(info,f)
            for name, data in frontend.items():
                info=tarfile.TarInfo(name);info.mode=0o644;info.size=len(data)
                tar.addfile(info,io.BytesIO(data))
        ssh(host,f'install -d -m 700 {base}/staging; tar xzf - -C {base}/staging; cp -r {base}/staging/public/assets {base}/public/; cp {base}/staging/public/index.html {base}/public/index.html.next; mv {base}/public/index.html.next {base}/public/index.html; cp -r {base}/staging/session/. {base}/session/; cp {base}/staging/auth.key {base}/auth.key; for f in {base}/staging/bin/*; do mv -f "$f" {base}/bin/; done',buffer.getvalue())
        put(host,base+'/peers.json',json.dumps(peers));put(host,base+'/feeds.json',json.dumps(feeds))
        environment={'LIGHT_DEX_QUIET':'1','LIGHT_DEX_AUTH_KEY':(session/'auth.key').read_text().strip(),'LIGHT_DEX_FAST_CERT_HANDOFF':'1','LIGHT_DEX_CHECKPOINT_INTERVAL':'1000','LIGHT_DEX_JOURNAL_MAX_BYTES':str(256*1024*1024),'LIGHT_DEX_FEED_PORT_BASE':'49000','LIGHT_DEX_REVISION':subprocess.check_output(['git','rev-parse','--short','HEAD'],cwd=ROOT,text=True).strip()+'+hosted'}
        if args.compact_proposals:environment['LIGHT_DEX_COMPACT_PROPOSALS']='1'
        put(host,base+'/runtime.env',''.join(f'{k}={v}\n' for k,v in environment.items()))
        service_dir=fact['home']+'/.config/systemd/user'
        put(host,service_dir+'/light-dex-feeds.service',unit('Light DEX shared exchange feeds',base,f'{base}/bin/light-dex-market-data-hub --feeds-file {base}/feeds.json','160M'))
        services=['light-dex-feeds']
        for i in ids:
            nodeenv=base+f'/node-{i}.env';put(host,nodeenv,f'LIGHT_DEX_READ_LISTEN={"127.0.0.1" if i<2 else fact["ip"]}:{48000+i}\n')
            argv=[base+'/bin/light_dex','--node',str(i),'--listen',peers[str(i)],'--advertise',peers[str(i)],'--consensus','hotstuff2','--recover-hotstuff2','--peers-file',base+'/peers.json','--chain-config',base+'/session/manifest.json','--validator-key',base+f'/session/keys/validator-{i}.key','--data-dir',base+'/data','--block-time-ms','350','--timeout-ms','3000','--primary-probe-timeout-ms','1500','--sync-timeout-ms','10000','--tx-batch-window-ms','10']
            for index,feed in enumerate(feeds):argv+=['--oracle-feed',f'{feed["base_asset"]}:0:{feed["venue"]}:ws://127.0.0.1:{49000+index}']
            content=unit(f'Light DEX validator {i}',base,shlex.join(argv),'384M').replace('EnvironmentFile='+base+'/runtime.env','EnvironmentFile='+base+'/runtime.env\nEnvironmentFile='+nodeenv)
            put(host,service_dir+f'/light-dex-validator-{i}.service',content);services.append(f'light-dex-validator-{i}')
        if host=='tokyo-ls':
            config={'manifest':base+'/session/manifest.json','auth_key_file':base+'/auth.key','database':base+'/public.sqlite','frontend_dir':base+'/public','public_origin':origin,'nodes':nodes,'writes_enabled':args.enable_writes,'extra_agent_pairs_per_market':1}
            put(host,base+'/gateway.json',json.dumps(config))
            for name,cmd,mem in [('gateway',base+'/bin/light-dex-hosted '+base+'/gateway.json','256M'),('liquidity',base+'/bin/light-dex-hosted '+base+'/gateway.json bots','192M')]:
                put(host,service_dir+f'/light-dex-{name}.service',unit('Light DEX '+name,base,cmd,mem));services.append('light-dex-'+name)
        else:
            # Inhibit system sleep without replacing any login or power configuration.
            put(host,service_dir+'/light-dex-awake.service',unit('Keep validator host awake',base,'/usr/bin/sleep infinity','16M',inhibit=True));services.append('light-dex-awake')
        ssh(host,'systemctl --user daemon-reload; systemctl --user enable --now '+' '.join(s+'.service' for s in services))
    # Apply gateway configuration changes without restarting any validator.
    ssh('tokyo-ls','systemctl --user restart light-dex-gateway')
    # No private endpoint in the public file. The origin is Tokyo's public IP only.
    (LOCAL/'deployment.json').write_text(json.dumps({'origin':origin,'hosts':facts},indent=2));os.chmod(LOCAL/'deployment.json',0o600)
    if args.publish:
        caddy=f'''{origin.removeprefix('https://')} {{
    encode zstd gzip
    header {{
        X-Content-Type-Options nosniff
        Referrer-Policy no-referrer
        X-Frame-Options DENY
        Content-Security-Policy "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; connect-src 'self' wss://{origin.removeprefix('https://')}; img-src 'self' data:; object-src 'none'; frame-ancestors 'none'; base-uri 'self'"
    }}
    @status path /api/v1/network
    @document path / /index.html
    header @document Cache-Control no-cache
    @assets path /assets/*
    header @assets Cache-Control "public, max-age=31536000, immutable"
    header @status Access-Control-Allow-Origin https://nicobecodin.github.io
    reverse_proxy 127.0.0.1:1422 {{
        header_up X-Real-IP {{remote_host}}
    }}
}}
'''
        put('tokyo-ls',facts['tokyo-ls']['base']+'/public.caddy',caddy)
        command=r'''sudo install -d -m 755 /etc/caddy/light-dex
sudo cp /etc/caddy/Caddyfile /etc/caddy/Caddyfile.before-light-dex
if sudo test -f /etc/caddy/light-dex/public.caddy; then sudo cp /etc/caddy/light-dex/public.caddy /etc/caddy/light-dex/public.before; else sudo install -m 644 /dev/null /etc/caddy/light-dex/public.before; fi
rollback() { sudo cp /etc/caddy/Caddyfile.before-light-dex /etc/caddy/Caddyfile; sudo cp /etc/caddy/light-dex/public.before /etc/caddy/light-dex/public.caddy; }
trap rollback ERR
sudo install -m 644 "$HOME/.local/share/light-dex-public/public.caddy" /etc/caddy/light-dex/public.caddy
if ! sudo grep -q '^import /etc/caddy/light-dex/\*.caddy' /etc/caddy/Caddyfile; then printf '\nimport /etc/caddy/light-dex/*.caddy\n' | sudo tee -a /etc/caddy/Caddyfile >/dev/null; fi
sudo caddy validate --config /etc/caddy/Caddyfile >/dev/null 2>&1
sudo systemctl reload caddy
trap - ERR'''
        ssh('tokyo-ls','set -e\n'+command)
    print('Installed four validators and supervised gateway/liquidity. Endpoint saved in hosted/.local/deployment.json.')
if __name__=='__main__':main()

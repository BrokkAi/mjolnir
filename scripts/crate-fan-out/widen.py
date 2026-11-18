#!/usr/bin/env python3
"""After a move, widen items that a crate above now needs: loop over privacy errors and make the named definitions `pub`."""
import json,re,subprocess,sys,collections
# usage: widen.py <directory-prefix-of-the-lower-crate/> <package>...
ONLY=sys.argv[1]; pkgs=sys.argv[2:]
VIS=re.compile(r'pub\((?:crate|super|in [a-z_:]+)\) ')
for rnd in range(12):
    out=subprocess.run(['cargo','check','--all-targets','--message-format=json']+sum((['-p',p] for p in pkgs),[]),capture_output=True,text=True).stdout
    edits=collections.defaultdict(set); other=collections.Counter()
    for line in out.splitlines():
        try: m=json.loads(line)
        except ValueError: continue
        if m.get('reason')!='compiler-message': continue
        d=m['message']
        if d['level']!='error': continue
        code=(d.get('code') or {}).get('code')
        hit=False
        if code in ('E0603','E0624','E0616','E0451','E0423','E0433','E0432'):
            for c in [d]+d['children']:
                for s in c['spans']:
                    if s.get('text') and VIS.search(s['text'][0]['text']) and s['file_name'].startswith(ONLY):
                        edits[s['file_name']].add(s['line_start']); hit=True
        if not hit: other[(code,d['message'][:90],d['spans'][0]['file_name']+':'+str(d['spans'][0]['line_start']) if d['spans'] else '')]+=1
    n=0
    for f,lines in edits.items():
        L=open(f).read().split('\n')
        for ln in lines:
            new=VIS.sub('pub ',L[ln-1],count=1)
            if new!=L[ln-1]: L[ln-1]=new; n+=1
        open(f,'w').write('\n'.join(L))
    print('round',rnd,'widened',n,'other errors',sum(other.values()))
    if n==0:
        for k,v in list(other.items())[:25]: print('  ',v,k)
        break

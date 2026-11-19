#!/usr/bin/env python3
"""Move a set of top-level modules out of one crate into a new one, re-exporting
them from the old crate under their old names. Driven by a JSON spec with the
keys from, dir, lib, package, after_member, modules, files, doc, reexports and
manifest; run it once without and once with the extra argument `tail` (the
second run edits the workspace and the old crate's manifests). It assumes the
old crate already depends on mj-store and has test-hooks and test-support
features, as mj-controller does."""
import json,os,re,subprocess,sys
spec=json.load(open(sys.argv[1]))
src=spec['from']; new=spec['dir']; lib=spec['lib']
def rd(p): return open(p).read()
def wr(p,s): open(p,'w').write(s)
TAIL=len(sys.argv)>2
if not TAIL:
    os.makedirs(new+'/src',exist_ok=True)
    for m in spec['modules']:
        for f in (f'{src}/src/{m}',f'{src}/src/{m}.rs'):
            if os.path.exists(f): subprocess.run(['git','mv',f,new+'/src/'],check=True)
    for a,b in spec.get('files',[]):
        os.makedirs(os.path.dirname(new+'/'+b),exist_ok=True)
        subprocess.run(['git','mv',src+'/'+a,new+'/'+b],check=True)
    if not os.path.lexists(new+'/LICENSE'): os.symlink('../LICENSE',new+'/LICENSE')
    n=0
    for root,_,fs in os.walk(new+'/src'):
        for f in fs:
            if f.endswith('.rs'):
                p=os.path.join(root,f); s=rd(p); n+=s.count('pub(crate) '); wr(p,s.replace('pub(crate) ','pub '))
    print('pub(crate) widened:',n)
    l=rd(src+'/src/lib.rs'); decl=[]
    for m in spec['modules']:
        mt=re.search(r'((?:#\[cfg\([^\n]*\)\]\n)?)(pub |pub\(crate\) )?mod '+m+r';\n',l); assert mt,m
        cfg,vis=mt.group(1),mt.group(2) or ''
        decl.append(cfg+'pub mod '+m+';\n')
        l=l[:mt.start()]+cfg+(vis if vis else 'pub(crate) ')+f'use {lib}::'+m+';\n'+l[mt.end():]
    wr(src+'/src/lib.rs',l)
    wr(new+'/src/lib.rs',spec['doc']+'''
    // `test-support` compiles the test-only items for the tests of the crates
    // above, which use only some of them. The dead-code lints still run on normal
    // and test builds.
    #![cfg_attr(
        all(feature = "test-support", not(test)),
        allow(dead_code, unused_imports)
    )]

    // The moved modules name these by their old paths.
    '''+spec['reexports']+'\n'+''.join(decl))
    wr(new+'/Cargo.toml',spec['manifest'])

if not TAIL: sys.exit(0)
r=rd('Cargo.toml'); key=new
anchor=spec['after_member']
r=re.sub(r'^((?:default-)?members = \[.*?"'+re.escape(anchor)+r'",)',lambda m:m.group(1)+f' "{key}",',r,flags=re.M); assert r.count(f'"{key}"')==2,r.count(f'"{key}"')
i=r.index('mj-controller = { path = "mj-controller"'); ver=re.search(r'version = "([^"]+)"',r[i:]).group(1)
r=r[:i]+f'{key} = {{ path = "{key}", package = "{spec["package"]}", version = "{ver}" }}\n'+r[i:]; wr('Cargo.toml',r)
c=rd(src+'/Cargo.toml')
m=re.search(r'test-hooks = \[([^\]]*)\]',c); c=c.replace(m.group(0),f'test-hooks = [{m.group(1)}, "{key}/test-hooks"]')
m=re.search(r'test-support = \[([^\]]*)\]',c); c=c.replace(m.group(0),f'test-support = [{m.group(1)}, "{key}/test-support"]')
c=c.replace('mj-store.workspace = true\n',f'mj-store.workspace = true\n{key}.workspace = true\n',1)
a='mj-store = { workspace = true, features = ["test-support"] }\n'; assert a in c
c=c.replace(a,a+f'{key} = {{ workspace = true, features = ["test-support"] }}\n'); wr(src+'/Cargo.toml',c)

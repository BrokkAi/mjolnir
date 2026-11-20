#!/usr/bin/env python3
"""Move top-level items (functions, structs, enums, consts, impls) from one
Rust file to the end of another, with their doc comments and attributes, and
make them `pub`:
  extract-items.py <from.rs> <to.rs> 'fn name' 'struct Name' 'impl Name' ...
Each item is found by its header at column 0 and ends at the next line that
is `}` (or, for a one-line item, at its `;`). Imports are left to the caller."""
import re,sys
src,dst,names=sys.argv[1],sys.argv[2],sys.argv[3:]
lines=open(src).read().split('\n'); out=[]
for name in names:
    pat=re.compile(r'^(pub(\([^)]*\))? )?(async )?(unsafe )?'+re.escape(name)+r'\b')
    hits=[i for i,l in enumerate(lines) if pat.match(l)]
    assert len(hits)==1,(name,hits)
    s=e=hits[0]
    while s>0 and (lines[s-1].startswith('///') or lines[s-1].startswith('#[') or lines[s-1].startswith('// ')): s-=1
    if not (lines[e].rstrip().endswith(';') and '{' not in lines[e]):
        while lines[e]!='}' and not re.match(r'^\}[;,]?$',lines[e]) and not (lines[e].startswith(')') and lines[e].endswith(';')): e+=1
    item=lines[s:e+1]
    h=hits[0]-s
    item[h]=re.sub(r'^(pub(\([^)]*\))? )?','' if name.startswith('impl') else 'pub ',item[h],count=1)
    out+=['']+item
    del lines[s:e+1]
    if s<len(lines) and s>0 and lines[s]=='' and lines[s-1]=='': del lines[s]
open(src,'w').write('\n'.join(lines))
try: cur=open(dst).read().rstrip('\n')+'\n'
except FileNotFoundError: cur=''
open(dst,'w').write(cur+'\n'.join(out).lstrip('\n')+'\n')

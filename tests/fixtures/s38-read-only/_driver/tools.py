import json,sys
# prints: request count, tool names declared per request (deduped), and last tool-result text seen
reqs=[r for r in (json.loads(l) for l in open(sys.argv[1]) if l.strip()) if isinstance(r.get('body'),dict)]
def names(b):
    out=[]
    for t in b.get('tools') or []:
        if 'function' in t: out.append(t['function'].get('name'))
        elif 'name' in t: out.append(t['name'])
        elif 'functionDeclarations' in t: out+= [f['name'] for f in t['functionDeclarations']]
        elif 'type' in t: out.append(t['type'])
    return out
print('requests:',len(reqs))
seen=[]
for r in reqs:
    n=names(r['body'])
    if n and n not in seen: seen.append(n)
for n in seen: print('tools:',n)

import re, os, csv, sys
root = sys.argv[1]
main = os.path.join(root, 'src/main/java/com/chalkak/backend')
test = os.path.join(root, 'src/test/java/com/chalkak/backend')
out = sys.argv[2]
# endpoints
rows=[]
for dp,_,fs in os.walk(main):
    for f in fs:
        if not f.endswith('Controller.java'): continue
        p=os.path.join(dp,f); s=open(p,encoding='utf-8').read()
        consts=dict(re.findall(r'static final String (\w+)\s*=\s*"([^"]+)"',s))
        m=re.search(r'@RequestMapping\(([^)]*)\)',s)
        base=''
        if m:
            a=m.group(1).strip()
            if a.startswith('"'): base=a.strip('"')
            else: base=consts.get(a.split('.')[-1],a)
        for mm in re.finditer(r'@(Get|Post|Put|Patch|Delete)Mapping(?:\(([^)]*)\))?\s*(?:@[^\n]*\n\s*)*public\s+[\w<>?, ]+\s+(\w+)\(',s):
            verb,arg,name=mm.groups()
            sub=(arg or '').strip().strip('"')
            path=base+sub
            if path.startswith('/internal'): auth='HMAC 콜백'
            elif path in('/api/v1/admin/auth/login','/api/v1/admin/auth/refresh','/api/v1/admin/auth/logout'): auth='공개(관리자 인증 전)'
            elif path.startswith('/api/v1/admin'): auth='관리자 토큰'
            elif path.startswith('/api/v1/auth'): auth='공개'
            elif verb=='Get' and path in('/api/v1/topics','/api/v1/posts'): auth='익명 또는 회원'
            else: auth='회원 토큰'
            dom=os.path.relpath(p,main).split('/')[0]
            rows.append([dom,verb.upper(),path,f[:-5]+'#'+name,auth])
rows.sort(key=lambda r:(r[2],r[1]))
with open(os.path.join(out,'endpoints.csv'),'w',newline='',encoding='utf-8') as fh:
    w=csv.writer(fh); w.writerow(['domain','method','path','handler','auth']); w.writerows(rows)
print('endpoints',len(rows))
# tests
trows=[]
for dp,_,fs in os.walk(test):
    for f in fs:
        if not f.endswith('.java'): continue
        p=os.path.join(dp,f); s=open(p,encoding='utf-8').read()
        rel=os.path.relpath(p,test)
        if '@SpringBootTest' in s or 'IntegrationTestSupport' in s: kind='통합(Spring+DB)'
        elif '@WebMvcTest' in s: kind='웹 슬라이스'
        elif '@DataJpaTest' in s: kind='JPA 슬라이스'
        elif 'architecture' in rel: kind='아키텍처'
        else: kind='단위'
        lines=s.split('\n'); disp=None; ann=None
        for i,l in enumerate(lines):
            t=l.strip()
            d=re.match(r'@DisplayName\("(.*)"\)',t)
            if d: disp=d.group(1); continue
            if re.match(r'@(Test|ParameterizedTest)\b',t): ann=t.split('(')[0]; continue
            mm=re.match(r'(?:public\s+)?void\s+(\w+)\(',t)
            if mm and ann:
                trows.append([rel.split('/')[0],rel,mm.group(1),disp or '',kind,ann,'','미이식'])
                ann=None; disp=None
            elif t.startswith('class ') or 'class ' in t and '{' in t:
                disp=None
trows.sort()
with open(os.path.join(out,'java-test-inventory.csv'),'w',newline='',encoding='utf-8') as fh:
    w=csv.writer(fh); w.writerow(['domain','file','method','display_name','java_layer','annotation','rust_target','status']); w.writerows(trows)
from collections import Counter
print('tests',len(trows)); print(Counter(r[4] for r in trows))

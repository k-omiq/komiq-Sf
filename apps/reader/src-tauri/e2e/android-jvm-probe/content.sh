#!/bin/zsh
# Content path against an engine at 127.0.0.1:$1: store, extension install, browse,
# chapter list, page list, one page image. $2 = extension package, $3 = source name.
PORT=$1; PKG=${2:-eu.kanade.tachiyomi.extension.en.mangapill}; NAME=${3:-mangapill}; OUT=${4:-/tmp}
U=http://127.0.0.1:$PORT; AUTH=${ENGINE_AUTH:+-u $ENGINE_AUTH}
gql(){ curl -s -m 180 $AUTH -H 'Content-Type: application/json' -d "$1" $U/api/graphql; }
j(){ python3 -c "import json,sys;d=json.loads(sys.stdin.read(),strict=False);$1"; }
short(){ cut -c1-${1:-220} | tr '\n' ' '; echo; }
echo "1 addStore: $(gql '{"query":"mutation{addExtensionStore(input:{indexUrl:\"https://raw.githubusercontent.com/keiyoushi/extensions/repo/index.min.json\"}){extensionStore{name}}}"}' | short)"
echo "2 fetchExtensions: $(gql '{"query":"mutation{fetchExtensions(input:{}){extensions{pkgName}}}"}' | j "print(len(d['data']['fetchExtensions']['extensions']),'extensions')")"
echo "3 install: $(gql "{\"query\":\"mutation{updateExtension(input:{id:\\\"$PKG\\\",patch:{install:true}}){extension{pkgName isInstalled versionName}}}\"}" | short)"
SRC=$(gql "{\"query\":\"query{sources(filter:{name:{includesInsensitive:\\\"$NAME\\\"}}){nodes{id name lang}}}\"}"); echo "4 sources: $(echo "$SRC" | short)"
SID=$(echo "$SRC" | j "print(d['data']['sources']['nodes'][0]['id'])") || exit 1
BR=$(gql "{\"query\":\"mutation{fetchSourceManga(input:{source:\\\"$SID\\\",type:POPULAR,page:1}){hasNextPage mangas{id title}}}\"}"); echo "5 browse: $(echo "$BR" | short 400)"
MID=$(echo "$BR" | j "print(d['data']['fetchSourceManga']['mangas'][0]['id'])") || exit 1
CH=$(gql "{\"query\":\"mutation{fetchChapters(input:{mangaId:$MID}){chapters{id name}}}\"}"); echo "6 chapters: $(echo "$CH" | j "c=d['data']['fetchChapters']['chapters'];print(len(c),'chapters, first:',c[0])" 2>&1 | short 300)"
CID=$(echo "$CH" | j "print(d['data']['fetchChapters']['chapters'][0]['id'])") || { echo "$CH" | short 600; exit 1; }
PG=$(gql "{\"query\":\"mutation{fetchChapterPages(input:{chapterId:$CID}){pages}}\"}"); echo "7 pages: $(echo "$PG" | j "p=d['data']['fetchChapterPages']['pages'];print(len(p),'pages, first:',p[0])" 2>&1 | short 300)"
P0=$(echo "$PG" | j "print(d['data']['fetchChapterPages']['pages'][0])") || { echo "$PG" | short 600; exit 1; }
curl -s -m 120 $AUTH -o $OUT/page0.bin -w "8 image: http=%{http_code} bytes=%{size_download} type=%{content_type} time=%{time_total}s\n" "$U$P0"; file -b $OUT/page0.bin | short 120

export const repo = 'https://github.com/rxp200/zed-cn';
const names = new Map([['Zed-x86_64.exe',['Windows','x86_64']],['Zed-aarch64.exe',['Windows','ARM64']],['zed-linux-x86_64.tar.gz',['Linux','x86_64']],['zed-linux-aarch64.tar.gz',['Linux','ARM64']],['Zed-aarch64.dmg',['macOS','Apple Silicon']]]);
export function selectDownloads(feed,channel='stable') {
 if (!feed || feed.schema_version !== 1 || !Array.isArray(feed.releases)) throw new Error('发布清单格式不正确');
 const pattern = channel === 'dev' ? /^zed-cn-dev-v(\d+)\.(\d+)\.(\d+)-r([1-9]\d*)$/ : /^zed-cn-v(\d+)\.(\d+)\.(\d+)-r([1-9]\d*)$/;
 const releases=feed.releases.filter(r=>r && !r.draft && r.prerelease===(channel==='dev') && pattern.test(r.tag_name) && Array.isArray(r.assets)).sort((a,b)=>{const x=a.tag_name.match(pattern).slice(1).map(Number),y=b.tag_name.match(pattern).slice(1).map(Number); for(let i=0;i<4;i++){if(x[i]!==y[i])return y[i]-x[i]}return 0});
 const result=new Map();
 for(const release of releases) for(const asset of release.assets){
  if(!asset || !names.has(asset.name) || result.has(asset.name) || asset.state!=='uploaded' || !Number.isSafeInteger(asset.size) || asset.size<=0 || !/^[a-f0-9]{64}$/.test(asset.sha256||''))continue;
  const expected=`${repo}/releases/download/${release.tag_name}/${asset.name}`;
  if(asset.browser_download_url!==expected)continue;
  const [platform,architecture]=names.get(asset.name);
  result.set(asset.name,{...asset,platform,architecture,tag:release.tag_name,version:release.tag_name.replace(/^zed-cn-(?:dev-)?v/,''),releaseUrl:`${repo}/releases/tag/${release.tag_name}`});
 }
 return [...result.values()];
}
export function formatSize(size){return `${(size/1024/1024).toFixed(1)} MiB`}

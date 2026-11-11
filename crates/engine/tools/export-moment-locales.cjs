// Reproduce derived MIT Moment 2.30.1 locale tables; no JS executes in product.
// node export-moment-locales.cjs /absolute/moment-with-locales.min.js output.json
const fs=require('fs');const path=require('path');
if(!process.argv[2]||!process.argv[3])throw new Error('Pass bundled Moment path and output path');
const moment=require(path.resolve(process.argv[2]));
if(moment.version!=='2.30.1')throw new Error('Expected Moment 2.30.1');
if(process.env.TZ!=='UTC')throw new Error('Run with TZ=UTC for era boundaries');
const destination=process.argv[3];
const contexts=['MMMM','D MMMM','MMMM D','MMM','D MMM','-MMM-','dddd','[В прошлую] dddd','[У мінулую] dddd','[წინა] dddd','dddd HH:mm','[У] dddd','[Минулої] dddd'];
let localeFunctions=[];const data=moment.locales().map(id=>{let l=moment.localeData(id);let functions={};for(let k of ['months','monthsShort','weekdays','weekdaysShort','weekdaysMin','ordinal','eraYearOrdinal','meridiem','postformat'])if(typeof l._config[k]==='function')functions[k]=l._config[k].toString();localeFunctions.push({id,functions});
const months=(method,format)=>Array.from({length:12},(_,i)=>l[method](moment.utc([2026,i,15]),format));const days=(method,format)=>Array.from({length:7},(_,i)=>l[method](moment.utc([2026,2,1+i]),format));
const compressOrd=(token)=>{let out=[];for(let i=0;i<10000;i++){let value=String(l.ordinal(i,token)),s=String(i),at=value.indexOf(s);out.push(at<0?value:[value.slice(0,at),value.slice(at+s.length)]);}let period=[1,10,100].find(p=>out.slice(100).every((v,i)=>JSON.stringify(v)===JSON.stringify(out[100+(i%p)])))??10000;
let cycle=out.slice(period===10000?0:100,period===10000?10000:100+period);let exceptions={};for(let i=0;i<Math.min(100,out.length);i++)if(JSON.stringify(out[i])!==JSON.stringify(cycle[i%period]))exceptions[i]=out[i];return {cycle,exceptions};};
let profiles=[],ordinals={};for(let token of ['M','Q','D','DDD','d','w','W','y']){let p=compressOrd(token);let i=profiles.findIndex(v=>JSON.stringify(v)===JSON.stringify(p));if(i<0){i=profiles.length;profiles.push(p);}ordinals[token]=i;}
let meridiem=[];for(let i=0;i<1440;i++){let pair=[l.meridiem(Math.floor(i/60),i%60,false),l.meridiem(Math.floor(i/60),i%60,true)];if(!meridiem.length||JSON.stringify(meridiem.at(-1).labels)!==JSON.stringify(pair))meridiem.push({start:i,labels:pair});}
let post={};for(let c of Array.from({length:128},(_,i)=>String.fromCharCode(i))){let v=l.postformat(c);if(v!==c)post[c]=v;}
return {id,months:contexts.slice(0,3).map(format=>({format,names:months('months',format)})),monthsShort:contexts.slice(3,6).map(format=>({format,names:months('monthsShort',format)})),weekdays:contexts.slice(6).map(format=>({format,names:days('weekdays',format)})),weekdaysShort:days('weekdaysShort','ddd'),weekdaysMin:days('weekdaysMin','dd'),week:l._week,presets:Object.fromEntries(['LT','LTS','L','LL','LLL','LLLL','l','ll','lll','llll'].map(k=>[k,l.longDateFormat(k)])),ordinalProfiles:profiles,ordinals,meridiem,postformat:post,eras:l.eras().map(e=>({...e,since:String(e.since),until:String(e.until)})),monthRule:l._months?.isFormat?.source??null,monthShortRule:l._monthsShort?.isFormat?.source??null,weekdayRule:l._weekdays?.isFormat?.source??null};});
fs.writeFileSync(destination,JSON.stringify(data)+'\n');fs.writeFileSync(destination+'.functions.json',JSON.stringify(localeFunctions,null,2)+'\n');console.log(data.length,fs.statSync(destination).size);

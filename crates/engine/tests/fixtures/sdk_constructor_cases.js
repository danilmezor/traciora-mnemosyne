(() => {
  const sdk = globalThis.__ownedSdk;
  const vault = {ownedVault: true}, marker = {ownedMarker: true};
  const units = s => Array.from({length:s.length},(_,i)=>s.charCodeAt(i));
  let pathInput;
  const value = v => v === vault ? {identity:'vault'} : v === marker ? {identity:'marker'} : v === pathInput && typeof v === 'object' && v !== null ? {identity:'path'} :
    v === null ? {type:'null'} : typeof v === 'undefined' ? {type:'undefined'} :
    typeof v === 'string' ? {type:'string',units:units(v)} :
    typeof v === 'number' ? {type:'number',value:Object.is(v,-0)?'-0':String(v)} :
    typeof v === 'boolean' ? {type:'boolean',value:v} : Array.isArray(v) ? {type:'Array',length:v.length} : {type:typeof v};
  const snapshot = o => {
    if (o === null || (typeof o !== 'object' && typeof o !== 'function')) return value(o);
    return Object.entries(Object.getOwnPropertyDescriptors(o)).map(([key,d])=>({key,enumerable:d.enumerable,configurable:d.configurable,
      ...(Object.hasOwn(d,'value')?{writable:d.writable,value:value(d.value)}:{get:typeof d.get,set:typeof d.set})}));
  };
  const rows=[];
  function run(id,klass,operation,make) {
    const log=[];let receiver,result;
    const cfg=make(log);receiver=cfg.receiver;pathInput=cfg.path;
    try {
      if(operation==='construct') { result=Reflect.construct(sdk[klass],[vault,cfg.path],cfg.newTarget||sdk[klass]);receiver=result; }
      else if(operation==='call') result=Reflect.apply(sdk[klass],receiver,[vault,cfg.path]);
      else result=Reflect.apply(sdk[klass].prototype.setPath,receiver,[cfg.path]);
      rows.push({id,klass,operation,result:result===receiver?{identity:'receiver'}:value(result),own:snapshot(receiver),log});
    } catch(e) { rows.push({id,klass,operation,error:e===marker?{identity:'marker'}:{name:e.name,message:e.message},own:snapshot(receiver),log}); }
  }
  for(const klass of ['TAbstractFile','TFile','TFolder']) {
    for(const [label,path] of [['plain','Folder/UP.MD'],['root','/'],['empty',''],['dot','.hidden'],['trailing-dot','Folder/A.'],['lone','Folder/A.'+String.fromCharCode(0xd800)],['pair','Folder/A.𐐀']]) {
      run(klass+'-new-'+label,klass,'construct',()=>({path}));
    }
    run(klass+'-call-virtual',klass,'call',log=>({receiver:{setPath(p){log.push({call:'setPath',argument:value(p),own:snapshot(this)});this.path=marker;return marker;}},path:'Input.md'}));
    run(klass+'-call-getter-throws',klass,'call',log=>({receiver:Object.defineProperty({},'setPath',{get(){log.push('get:setPath');throw marker;},configurable:true}),path:'Input.md'}));
    run(klass+'-call-null',klass,'call',()=>({receiver:null,path:'Input.md'}));
    run(klass+'-call-undefined',klass,'call',()=>({receiver:undefined,path:'Input.md'}));
    run(klass+'-call-number',klass,'call',()=>({receiver:3,path:'Input.md'}));
    run(klass+'-call-no-setPath',klass,'call',()=>({receiver:{},path:'Input.md'}));
    run(klass+'-derived-override',klass,'construct',log=>{
      const C=class extends sdk[klass] {setPath(p){log.push({call:'override',argument:value(p),own:snapshot(this)});this.path='OVERRIDE';this.name='CUSTOM';return marker;}};
      return {newTarget:C,path:'Input.md'};
    });
  }
  for(const klass of ['TAbstractFile','TFile']) {
    for(const [label,path] of [['plain','Folder/UP.MD'],['missing',undefined],['null',null],['number',3],['lone','Folder/A.'+String.fromCharCode(0xd800)],['pair','Folder/A.𐐀']]) {
      run(klass+'-set-'+label,klass,'setPath',()=>({receiver:{},path}));
    }
    run(klass+'-set-pathlike',klass,'setPath',log=>({receiver:{},path:{lastIndexOf(arg){log.push({call:'lastIndexOf',arg});return -1;}}}));
    run(klass+'-set-pathlike-sliced',klass,'setPath',log=>({receiver:{},path:{lastIndexOf(arg){log.push({call:'lastIndexOf',arg});return 2;},slice(arg){log.push({call:'slice',arg});return 'A.MD';}}}));
    run(klass+'-set-index-coercion',klass,'setPath',log=>({receiver:{},path:{lastIndexOf(arg){log.push({call:'lastIndexOf',arg});return {valueOf(){log.push('valueOf:index');return 2;}};},slice(arg){log.push({call:'slice',arg});return 'A.MD';}}}));
    run(klass+'-set-proxy-writes',klass,'setPath',log=>({receiver:new Proxy({}, {set(o,k,v){log.push({set:k,value:value(v)});return Reflect.set(o,k,v);}}),path:'Folder/UP.MD'}));
    run(klass+'-set-name-getter',klass,'setPath',log=>({receiver:Object.defineProperty({},'name',{get(){log.push('get:name');return 'Getter.Z';},set(v){log.push({set:'name',value:value(v)});},enumerable:true,configurable:true}),path:'Folder/UP.MD'}));
    run(klass+'-set-frozen-path',klass,'setPath',()=>({receiver:Object.defineProperty({},'path',{value:'OLD',writable:false,enumerable:true,configurable:true}),path:'Folder/UP.MD'}));
  }
  return JSON.stringify(rows);
})()

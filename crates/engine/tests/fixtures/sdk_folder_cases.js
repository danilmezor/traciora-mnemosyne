(() => {
  const sdk=globalThis.__ownedSdk, rows=[], marker={owned:true};
  const units=s=>Array.from({length:s.length},(_,i)=>s.charCodeAt(i));
  const describe=v=>v===marker?{identity:'marker'}:v===null?{type:'null'}:typeof v==='undefined'?{type:'undefined'}:typeof v==='string'?{type:'string',units:units(v)}:typeof v==='number'?{type:'number',value:String(v)}:typeof v==='boolean'?{type:'boolean',value:v}:{type:typeof v};
  function run(id,method,make) {
    const log=[];const receiver=make(log);
    try{rows.push({id,method,result:describe(Reflect.apply(sdk.TFolder.prototype[method],receiver,[])),log});}
    catch(e){rows.push({id,method,error:e===marker?{identity:'marker'}:{name:e.name,message:e.message},log});}
  }
  for(const method of ['getFileCount','getFolderCount']) {
    run(method+'-empty',method,()=>({children:[]}));
    run(method+'-primitive-children',method,()=>({children:[null,undefined,3,{},'file']}));
    run(method+'-holes',method,()=>({children:new Array(3)}));
    run(method+'-missing',method,()=>({}));
    run(method+'-null-this',method,()=>null);
    run(method+'-length-getters',method,log=>({children:{get length(){log.push('length');return 2;},get 0(){log.push('0');return null;},get 1(){log.push('1');return {};}}}));
    run(method+'-changing-length',method,log=>{let i=0;return {children:{get length(){log.push('length');return i++===0?3:0;},get 0(){log.push('0');return null;}}};});
    run(method+'-length-coercion',method,log=>({children:{length:{valueOf(){log.push('valueOf:length');return 1;}},0:null}}));
    run(method+'-child-getter-throws',method,log=>({children:{length:1,get 0(){log.push('0');throw marker;}}}));
    run(method+'-nested',method,()=>{const top=new sdk.TFolder({},'Top'),sub=new sdk.TFolder({},'Top/Sub');sub.children.push(new sdk.TFile({},'Top/Sub/A.md'));top.children.push(sub,null);return top;});
    run(method+'-recursive-override',method,log=>{const folder=new sdk.TFolder({},'Sub');folder[method]=()=>{log.push('recursive');return '2';};return {children:[folder,null]};});
    run(method+'-recursive-coercion',method,log=>{const folder=new sdk.TFolder({},'Sub');folder[method]=()=>({valueOf(){log.push('valueOf:count');return 2;}});return {children:[folder,null]};});
    run(method+'-recursive-noncallable',method,()=>{const folder=new sdk.TFolder({},'Sub');folder[method]=3;return {children:[folder]};});
  }
  run('prefix-root','getParentPrefix',log=>({isRoot(){log.push('isRoot');return true;},get path(){log.push('path');throw marker;}}));
  run('prefix-nonroot','getParentPrefix',log=>({isRoot(){log.push('isRoot');return false;},get path(){log.push('path');return 'Folder';}}));
  run('prefix-missing-path','getParentPrefix',()=>({isRoot(){return false;}}));
  run('prefix-coercion','getParentPrefix',log=>({isRoot(){log.push('isRoot');return false;},path:{valueOf(){log.push('valueOf:path');return 3;}}}));
  run('prefix-truthy','getParentPrefix',()=>({isRoot(){return 'yes';}}));
  run('prefix-missing-method','getParentPrefix',()=>({}));
  run('prefix-null','getParentPrefix',()=>null);
  run('prefix-lone','getParentPrefix',()=>({isRoot(){return false;},path:String.fromCharCode(0xd800)}));
  const previous=Object.getOwnPropertyDescriptor(sdk.TFolder,Symbol.hasInstance);
  try {
    let calls=0;
    Object.defineProperty(sdk.TFolder,Symbol.hasInstance,{configurable:true,value(v){calls++;return v===marker;}});
    marker.getFileCount=()=>2;marker.getFolderCount=()=>3;
    run('file-count-hasInstance','getFileCount',()=>({children:[marker,null]}));
    run('folder-count-hasInstance','getFolderCount',()=>({children:[marker,null]}));
    rows.push({id:'hasInstance-calls',calls});
  } finally { if(previous)Object.defineProperty(sdk.TFolder,Symbol.hasInstance,previous);else delete sdk.TFolder[Symbol.hasInstance]; }
  return JSON.stringify(rows);
})()

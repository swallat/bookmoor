const { chromium } = require('playwright');
const fs=require('fs');
(async()=>{
 const b=await chromium.launch(); const p=await b.newPage({deviceScaleFactor:1});
 const svg=fs.readFileSync(process.argv[2],'utf8'); const out=process.argv[3];
 for(const s of [16,32,48,128]){
  await p.setViewportSize({width:s,height:s});
  await p.setContent(`<body style="margin:0;background:transparent"><div style="width:${s}px;height:${s}px">${svg.replace(/width="128" height="128"/,`width="${s}" height="${s}"`)}</div></body>`);
  await p.screenshot({path:`${out}-${s}.png`,omitBackground:true,clip:{x:0,y:0,width:s,height:s}});
 }
 await b.close();
})();

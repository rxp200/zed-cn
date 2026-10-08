import * as THREE from 'three';
export function mountSculpture(host){
 let renderer;try{renderer=new THREE.WebGLRenderer({alpha:true,antialias:true,powerPreference:'low-power'})}catch{return}
 renderer.setPixelRatio(Math.min(devicePixelRatio,1.5));renderer.domElement.setAttribute("aria-hidden","true");host.append(renderer.domElement);host.closest('.hero-art').querySelector('.orbit-fallback').style.opacity='.08';
 const scene=new THREE.Scene(),camera=new THREE.PerspectiveCamera(35,1,.1,100);camera.position.z=8;
 const group=new THREE.Group();group.rotation.set(.4,-.5,.25);scene.add(group);
 const geometry=new THREE.TorusGeometry(2,.54,18,90),wire=new THREE.WireframeGeometry(geometry),material=new THREE.LineBasicMaterial({color:0xb3cb79,transparent:true,opacity:.28});
 group.add(new THREE.LineSegments(wire,material));
 const ringGeometry=new THREE.TorusGeometry(2.8,.009,4,100),ringMaterial=new THREE.MeshBasicMaterial({color:0xd5ef83,transparent:true,opacity:.55}),ring=new THREE.Mesh(ringGeometry,ringMaterial);ring.rotation.x=1.1;group.add(ring);
 const dotsGeometry=new THREE.BufferGeometry();const points=[];for(let i=0;i<65;i++){const a=i*2.399963;const r=2.9+(i%7)*.13;points.push(Math.cos(a)*r,Math.sin(a)*r,(i%5-2)*.3)}dotsGeometry.setAttribute('position',new THREE.Float32BufferAttribute(points,3));const dotsMaterial=new THREE.PointsMaterial({size:.028,color:0xd5ef83,transparent:true,opacity:.6});group.add(new THREE.Points(dotsGeometry,dotsMaterial));
 const button=document.createElement('button');button.className='motion-toggle';button.textContent='暂停动效';button.setAttribute('aria-pressed','false');host.closest('.hero').append(button);
 let visible=true,paused=false,frame=0,last=0,disposed=false;const reduced=matchMedia('(prefers-reduced-motion: reduce)');
 function render(time){frame=0;if(disposed)return;const delta=last?Math.min((time-last)/1000,.06):0;last=time;if(!paused&&!reduced.matches){group.rotation.y+=delta*.095;group.rotation.z+=delta*.025}renderer.render(scene,camera);if(visible&&!document.hidden&&!paused&&!reduced.matches)frame=requestAnimationFrame(render)}
 function sync(){cancelAnimationFrame(frame);frame=0;last=0;if(!disposed&&visible&&!document.hidden)frame=requestAnimationFrame(render)}
 const resize=new ResizeObserver(()=>{const {width,height}=host.getBoundingClientRect();if(width&&height){renderer.setSize(width,height,false);camera.aspect=width/height;camera.updateProjectionMatrix();sync()}});resize.observe(host);
 const observer=new IntersectionObserver(([entry])=>{visible=entry.isIntersecting;sync()});observer.observe(host);
 button.addEventListener('click',()=>{paused=!paused;button.textContent=paused?'播放动效':'暂停动效';button.setAttribute('aria-pressed',String(paused));sync()});
 document.addEventListener('visibilitychange',sync);reduced.addEventListener('change',sync);
 window.addEventListener('pagehide',()=>{disposed=true;cancelAnimationFrame(frame);resize.disconnect();observer.disconnect();document.removeEventListener('visibilitychange',sync);reduced.removeEventListener('change',sync);for(const resource of [geometry,wire,material,ringGeometry,ringMaterial,dotsGeometry,dotsMaterial])resource.dispose();renderer.dispose()},{once:true});
 sync();
}

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use image::RgbImage;
use mediapipe::{FaceLandmarker, Image as MpImage, ModelSource, Timestamp};
use std::{path::PathBuf, thread};

#[derive(Clone, Debug)]
pub struct FaceTrack { pub bbox:(f32,f32,f32,f32), pub landmarks:Vec<(f32,f32,f32)> }

pub fn spawn_worker(rx:Receiver<RgbImage>, tx:Sender<(Vec<FaceTrack>,String)>){thread::spawn(move||face_ai_worker(rx,tx));}
fn face_ai_worker(rx:Receiver<RgbImage>,tx:Sender<(Vec<FaceTrack>,String)>){
 let model=face_landmarker_model_path();
 if !model.exists(){let _=tx.send((Vec::new(),"AR: downloading Face Landmarker model…".into()));if let Err(e)=download_face_landmarker_model(&model){let _=tx.send((Vec::new(),format!("AR unavailable: {e}")));return;}}
 let mut landmarker=match FaceLandmarker::builder(ModelSource::path(&model)).num_faces(std::num::NonZeroU32::new(4).unwrap()).min_face_detection_confidence(mediapipe::Confidence::new(0.5).unwrap()).min_face_presence_confidence(mediapipe::Confidence::new(0.5).unwrap()).min_tracking_confidence(mediapipe::IouThreshold::new(0.5).unwrap()).output_blendshapes(true).output_transformation_matrixes(true).build_for_video(){Ok(v)=>v,Err(e)=>{let _=tx.send((Vec::new(),format!("AR Face Landmarker load failed: {e}")));return;}};
 let _=tx.send((Vec::new(),"AR ready · dense face tracking".into())); let mut timestamp_ms=0i64;
 while let Ok(src)=rx.recv(){let max_w=640u32;let small=if src.width()>max_w{let h=((src.height() as f32)*max_w as f32/src.width() as f32) as u32;image::imageops::resize(&src,max_w,h.max(1),image::imageops::FilterType::Triangle)}else{src.clone()};let temp=std::env::temp_dir().join("4k-rust-camera-ar.png");if let Err(e)=small.save(&temp){let _=tx.try_send((Vec::new(),format!("AR frame preparation failed: {e}")));continue;}timestamp_ms+=33;let result:Result<Vec<FaceTrack>>=(||{let image=MpImage::from_file(&temp)?;let result=landmarker.detect_for_video(&image,Timestamp::from_millis(timestamp_ms))?;let sx=src.width() as f32/small.width().max(1) as f32;let sy=src.height() as f32/small.height().max(1) as f32;Ok(result.landmarks.into_iter().map(|face|{let points=face.iter().map(|p|(p.point.x(),p.point.y(),p.point.z())).collect::<Vec<_>>();let(mut min_x,mut min_y,mut max_x,mut max_y)=(1.0f32,1.0f32,0.0f32,0.0f32);for&(x,y,_)in&points{min_x=min_x.min(x);min_y=min_y.min(y);max_x=max_x.max(x);max_y=max_y.max(y);}FaceTrack{bbox:(min_x*small.width()as f32*sx,min_y*small.height()as f32*sy,(max_x-min_x)*small.width()as f32*sx,(max_y-min_y)*small.height()as f32*sy),landmarks:points}}).collect())})();let _=std::fs::remove_file(&temp);match result{Ok(tracks)=>{let count=tracks.len();let _=tx.try_send((tracks,format!("AR active · {count} face(s) · dense landmarks")));}Err(e)=>{let _=tx.try_send((Vec::new(),format!("AR inference failed: {e}")));}}}
}
fn download_face_landmarker_model(path:&PathBuf)->Result<()>{const URL:&str="https://storage.googleapis.com/mediapipe-models/face_landmarker/face_landmarker/float16/1/face_landmarker.task";if let Some(parent)=path.parent(){std::fs::create_dir_all(parent)?;}let mut response=ureq::get(URL).call().map_err(|e|anyhow::anyhow!("model download failed: {e}"))?;let bytes=response.body_mut().with_config().limit(30*1024*1024).read_to_vec().map_err(|e|anyhow::anyhow!("model download failed: {e}"))?;if bytes.len()<1_000_000{anyhow::bail!("downloaded Face Landmarker model is unexpectedly small");}let temp=path.with_extension("part");std::fs::write(&temp,bytes)?;std::fs::rename(temp,path)?;Ok(())}
fn face_landmarker_model_path()->PathBuf{let mut p=std::env::current_exe().unwrap_or_else(|_|PathBuf::from("."));p.pop();p.push("models");p.push("face_landmarker.task");p}

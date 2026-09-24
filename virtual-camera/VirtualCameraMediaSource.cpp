// Native Windows Media Foundation virtual-camera boundary.
// Implements a software camera source that reads the newest processed frame
// from the Rust process through the shared FrameRing mapping.
#include <windows.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mferror.h>
#include <mfvirtualcamera.h>
#include <mfobjects.h>
#include <ks.h>
#include <ksmedia.h>
#include <atomic>
#include <string>
#include <vector>
#include <algorithm>
#include "FrameRing.h"

#pragma comment(lib, "mfplat.lib")
#pragma comment(lib, "mfuuid.lib")
#pragma comment(lib, "ole32.lib")
#pragma comment(lib, "advapi32.lib")

static const CLSID CLSID_4KRustCameraVirtualSource =
{ 0x9a2d7c21, 0x3a5f, 0x4d6b, { 0x8e, 0x0d, 0x1b, 0x7f, 0x0a, 0x5b, 0x2c, 0x31 } };

static const wchar_t* VC_CLSID_STRING = L"{9A2D7C21-3A5F-4D6B-8E0D-1B7F0A5B2C31}";
static HMODULE g_module = nullptr;

template<class T>
static HRESULT QI(T* object, REFIID iid, void** out) {
    if (!out) return E_POINTER;
    *out = nullptr;
    return object->QueryInterface(iid, out);
}

class CameraStream;

class CameraSource final : public IMFMediaSourceEx {
    std::atomic<ULONG> refs{1};
    IMFMediaEventQueue* events = nullptr;
    IMFPresentationDescriptor* presentation = nullptr;
    IMFStreamDescriptor* streamDesc = nullptr;
    CameraStream* stream = nullptr;
    std::atomic<bool> stopped{false};
    std::atomic<bool> shutdown{false};
public:
    CameraSource() = default;
    ~CameraSource() override;
    HRESULT Initialize();
    HRESULT QueryInterface(REFIID riid, void** ppv) override;
    ULONG AddRef() override { return ++refs; }
    ULONG Release() override { ULONG r=--refs; if(!r) delete this; return r; }

    STDMETHODIMP BeginGetEvent(IMFAsyncCallback* cb,IUnknown* state) override { return events ? events->BeginGetEvent(cb,state) : MF_E_SHUTDOWN; }
    STDMETHODIMP EndGetEvent(IMFAsyncResult* r,IMFMediaEvent** e) override { return events ? events->EndGetEvent(r,e) : MF_E_SHUTDOWN; }
    STDMETHODIMP GetEvent(DWORD f,IMFMediaEvent** e) override { return events ? events->GetEvent(f,e) : MF_E_SHUTDOWN; }
    STDMETHODIMP QueueEvent(MediaEventType t,REFGUID g,HRESULT h,const PROPVARIANT* v) override { return events ? events->QueueEventParamVar(t,g,h,v) : MF_E_SHUTDOWN; }

    STDMETHODIMP CreatePresentationDescriptor(IMFPresentationDescriptor** pp) override;
    STDMETHODIMP GetCharacteristics(DWORD* p) override { if(!p) return E_POINTER; if(shutdown) return MF_E_SHUTDOWN; *p=MFMEDIASOURCE_IS_LIVE; return S_OK; }
    STDMETHODIMP Pause() override { return MF_E_INVALID_STATE_TRANSITION; }
    STDMETHODIMP Shutdown() override;
    STDMETHODIMP Start(IMFPresentationDescriptor* pd,const GUID*,const PROPVARIANT*) override;
    STDMETHODIMP Stop() override;
    STDMETHODIMP GetSourceAttributes(IMFAttributes** pp) override;
    STDMETHODIMP GetStreamAttributes(DWORD id,IMFAttributes** pp) override;
    STDMETHODIMP SetD3DManager(IUnknown*) override { return S_OK; }
};

class CameraStream final : public IMFMediaStream2 {
    std::atomic<ULONG> refs{1};
    CameraSource* parent;
    IMFMediaEventQueue* events=nullptr;
    IMFStreamDescriptor* descriptor=nullptr;
    std::atomic<MF_STREAM_STATE> state{MF_STREAM_STATE_STOPPED};
    std::atomic<bool> shutdown{false};
public:
    explicit CameraStream(CameraSource* p):parent(p){parent->AddRef();}
    ~CameraStream() override {if(parent) parent->Release(); if(events)events->Release(); if(descriptor)descriptor->Release();}
    HRESULT Initialize(IMFStreamDescriptor* d);
    HRESULT QueryInterface(REFIID riid,void** ppv) override {
        if(!ppv)return E_POINTER;*ppv=nullptr;
        if(riid==IID_IUnknown||riid==IID_IMFMediaEventGenerator||riid==IID_IMFMediaStream||riid==IID_IMFMediaStream2){*ppv=static_cast<IMFMediaStream2*>(this);AddRef();return S_OK;}
        return E_NOINTERFACE;
    }
    ULONG AddRef()override{return ++refs;}
    ULONG Release()override{ULONG r=--refs;if(!r)delete this;return r;}
    STDMETHODIMP BeginGetEvent(IMFAsyncCallback* c,IUnknown* s)override{return events?events->BeginGetEvent(c,s):MF_E_SHUTDOWN;}
    STDMETHODIMP EndGetEvent(IMFAsyncResult* r,IMFMediaEvent**e)override{return events?events->EndGetEvent(r,e):MF_E_SHUTDOWN;}
    STDMETHODIMP GetEvent(DWORD f,IMFMediaEvent**e)override{return events?events->GetEvent(f,e):MF_E_SHUTDOWN;}
    STDMETHODIMP QueueEvent(MediaEventType t,REFGUID g,HRESULT h,const PROPVARIANT*v)override{return events?events->QueueEventParamVar(t,g,h,v):MF_E_SHUTDOWN;}
    STDMETHODIMP GetMediaSource(IMFMediaSource** pp)override;
    STDMETHODIMP GetStreamDescriptor(IMFStreamDescriptor** pp)override;
    STDMETHODIMP RequestSample(IUnknown* token)override;
    STDMETHODIMP SetStreamState(MF_STREAM_STATE s)override;
    STDMETHODIMP GetStreamState(MF_STREAM_STATE* s)override{if(!s)return E_POINTER;if(shutdown)return MF_E_SHUTDOWN;*s=state.load();return S_OK;}
    HRESULT Shutdown();
};

CameraSource::~CameraSource(){if(stream)stream->Release();if(presentation)presentation->Release();if(streamDesc)streamDesc->Release();if(events)events->Release();}
HRESULT CameraSource::QueryInterface(REFIID riid,void**ppv){if(!ppv)return E_POINTER;*ppv=nullptr;if(riid==IID_IUnknown||riid==IID_IMFMediaEventGenerator||riid==IID_IMFMediaSource||riid==IID_IMFMediaSourceEx){*ppv=static_cast<IMFMediaSourceEx*>(this);AddRef();return S_OK;}return E_NOINTERFACE;}

HRESULT CameraSource::Initialize(){
    HRESULT hr=MFCreateEventQueue(&events);if(FAILED(hr))return hr;
    IMFMediaType* type=nullptr;hr=MFCreateMediaType(&type);if(FAILED(hr))return hr;
    type->SetGUID(MF_MT_MAJOR_TYPE,MFMediaType_Video);type->SetGUID(MF_MT_SUBTYPE,MFVideoFormat_RGB32);
    MFSetAttributeSize(type,MF_MT_FRAME_SIZE,VC_WIDTH,VC_HEIGHT);MFSetAttributeRatio(type,MF_MT_FRAME_RATE,30,1);
    MFSetAttributeRatio(type,MF_MT_PIXEL_ASPECT_RATIO,1,1);type->SetUINT32(MF_MT_INTERLACE_MODE,MFVideoInterlace_Progressive);
    hr=MFCreateStreamDescriptor(0,1,type,&streamDesc);type->Release();if(FAILED(hr))return hr;
    stream=new(std::nothrow)CameraStream(this);if(!stream)return E_OUTOFMEMORY;
    hr=stream->Initialize(streamDesc);if(FAILED(hr))return hr;
    hr=MFCreatePresentationDescriptor(1,&streamDesc,&presentation);if(FAILED(hr))return hr;
    return S_OK;
}
HRESULT CameraSource::CreatePresentationDescriptor(IMFPresentationDescriptor**pp){if(!pp)return E_POINTER;*pp=nullptr;if(shutdown)return MF_E_SHUTDOWN;return presentation->Clone(pp);}
HRESULT CameraSource::Start(IMFPresentationDescriptor*pd,const GUID*,const PROPVARIANT*){if(shutdown)return MF_E_SHUTDOWN;if(!pd)return E_POINTER;BOOL sel=FALSE;IMFStreamDescriptor*d=nullptr;HRESULT hr=pd->GetStreamDescriptorByIndex(0,&sel,&d);if(d)d->Release();if(FAILED(hr)||!sel)return MF_E_INVALIDREQUEST;hr=stream->SetStreamState(MF_STREAM_STATE_RUNNING);if(FAILED(hr))return hr;stopped=false;return events->QueueEventParamVar(MESourceStarted,GUID_NULL,S_OK,nullptr);}
HRESULT CameraSource::Stop(){if(shutdown)return MF_E_SHUTDOWN;stopped=true;HRESULT hr=stream->SetStreamState(MF_STREAM_STATE_STOPPED);if(FAILED(hr))return hr;return events->QueueEventParamVar(MESourceStopped,GUID_NULL,S_OK,nullptr);}
HRESULT CameraSource::Shutdown(){if(shutdown.exchange(true))return S_OK;if(stream)stream->Shutdown();if(events)events->Shutdown();return S_OK;}
HRESULT CameraSource::GetSourceAttributes(IMFAttributes**pp){if(!pp)return E_POINTER;*pp=nullptr;if(shutdown)return MF_E_SHUTDOWN;IMFAttributes*a=nullptr;HRESULT hr=MFCreateAttributes(&a,4);if(SUCCEEDED(hr))a->SetUINT32(MF_DEVICESTREAM_FRAMESERVER_SHARED,1);if(SUCCEEDED(hr))hr=a->QueryInterface(IID_PPV_ARGS(pp));if(a)a->Release();return hr;}
HRESULT CameraSource::GetStreamAttributes(DWORD id,IMFAttributes**pp){if(id!=0)return MF_E_INVALIDSTREAMNUMBER;if(!pp)return E_POINTER;*pp=nullptr;IMFAttributes*a=nullptr;HRESULT hr=MFCreateAttributes(&a,4);if(SUCCEEDED(hr))a->SetUINT32(MF_DEVICESTREAM_FRAMESERVER_SHARED,1);if(SUCCEEDED(hr))hr=a->QueryInterface(IID_PPV_ARGS(pp));if(a)a->Release();return hr;}

HRESULT CameraStream::Initialize(IMFStreamDescriptor*d){HRESULT hr=MFCreateEventQueue(&events);if(FAILED(hr))return hr;descriptor=d;descriptor->AddRef();return S_OK;}
HRESULT CameraStream::GetMediaSource(IMFMediaSource**pp){if(!pp)return E_POINTER;*pp=nullptr;if(shutdown)return MF_E_SHUTDOWN;return parent->QueryInterface(IID_PPV_ARGS(pp));}
HRESULT CameraStream::GetStreamDescriptor(IMFStreamDescriptor**pp){if(!pp)return E_POINTER;*pp=nullptr;if(shutdown)return MF_E_SHUTDOWN;return descriptor->QueryInterface(IID_PPV_ARGS(pp));}
HRESULT CameraStream::SetStreamState(MF_STREAM_STATE s){if(shutdown)return MF_E_SHUTDOWN;if(s==MF_STREAM_STATE_PAUSED||s==MF_STREAM_STATE_RUNNING||s==MF_STREAM_STATE_STOPPED){state=s;if(events){if(s==MF_STREAM_STATE_RUNNING)events->QueueEventParamVar(MEStreamStarted,GUID_NULL,S_OK,nullptr);if(s==MF_STREAM_STATE_STOPPED)events->QueueEventParamVar(MEStreamStopped,GUID_NULL,S_OK,nullptr);}return S_OK;}return E_INVALIDARG;}
HRESULT CameraStream::Shutdown(){if(shutdown.exchange(true))return S_OK;state=MF_STREAM_STATE_STOPPED;if(events)events->Shutdown();return S_OK;}

static bool ReadLatestFrame(std::vector<BYTE>&out,ULONGLONG&timestamp){
    static HANDLE mapping=nullptr,mutexHandle=nullptr;static FrameRing*ring=nullptr;
    if(!mapping)mapping=OpenFileMappingW(FILE_MAP_READ,FALSE,VC_MAPPING_NAME);
    if(!mutexHandle)mutexHandle=OpenMutexW(SYNCHRONIZE|MUTEX_MODIFY_STATE,FALSE,VC_MUTEX_NAME);
    if(!mapping||!mutexHandle)return false;
    if(!ring)ring=(FrameRing*)MapViewOfFile(mapping,FILE_MAP_READ,0,0,sizeof(FrameRing));
    if(!ring)return false;
    if(WaitForSingleObject(mutexHandle,20)!=WAIT_OBJECT_0)return false;
    timestamp=ring->header.timestamp100ns;out.assign(ring->pixels,ring->pixels+VC_BYTES);ReleaseMutex(mutexHandle);return true;
}
HRESULT CameraStream::RequestSample(IUnknown*token){
    if(shutdown)return MF_E_SHUTDOWN;if(state.load()!=MF_STREAM_STATE_RUNNING)return MF_E_INVALIDREQUEST;
    std::vector<BYTE>pixels;ULONGLONG ts=0;if(!ReadLatestFrame(pixels,ts))pixels.assign(VC_BYTES,0);
    IMFMediaBuffer*buffer=nullptr;HRESULT hr=MFCreateMemoryBuffer(VC_BYTES,&buffer);if(FAILED(hr))return hr;
    BYTE*dst=nullptr;DWORD maxLen=0,curLen=0;hr=buffer->Lock(&dst,&maxLen,&curLen);if(FAILED(hr)){buffer->Release();return hr;}
    CopyMemory(dst,pixels.data(),min<size_t>(pixels.size(),maxLen));buffer->Unlock();buffer->SetCurrentLength(VC_BYTES);
    IMFSample*sample=nullptr;hr=MFCreateSample(&sample);if(SUCCEEDED(hr))hr=sample->AddBuffer(buffer);buffer->Release();if(FAILED(hr)){if(sample)sample->Release();return hr;}
    sample->SetSampleTime(ts?ts:MFGetSystemTime());sample->SetSampleDuration(333333);if(token)sample->SetUnknown(MFSampleExtension_Token,token);
    hr=events->QueueEventParamUnk(MEMediaSample,GUID_NULL,S_OK,sample);sample->Release();return hr;
}

class Activator final:public IMFActivate{
    std::atomic<ULONG>refs{1};IMFAttributes*attrs=nullptr;
public:
    ~Activator(){if(attrs)attrs->Release();}
    HRESULT Initialize(){return MFCreateAttributes(&attrs,4);}
    HRESULT QueryInterface(REFIID riid,void**ppv)override{if(!ppv)return E_POINTER;*ppv=nullptr;if(riid==IID_IUnknown||riid==IID_IMFAttributes||riid==IID_IMFActivate){*ppv=static_cast<IMFActivate*>(this);AddRef();return S_OK;}return E_NOINTERFACE;}
    ULONG AddRef()override{return ++refs;}ULONG Release()override{ULONG r=--refs;if(!r)delete this;return r;}
    HRESULT ActivateObject(REFIID riid,void**ppv)override{if(!ppv)return E_POINTER;*ppv=nullptr;CameraSource*s=new(std::nothrow)CameraSource();if(!s)return E_OUTOFMEMORY;HRESULT hr=s->Initialize();if(SUCCEEDED(hr))hr=s->QueryInterface(riid,ppv);s->Release();return hr;}
    HRESULT ShutdownObject()override{return S_OK;}HRESULT DetachObject()override{return S_OK;}
#define G(n,s,c) HRESULT n s override{if(!attrs)return E_UNEXPECTED;return attrs->c;}
    G(GetItem,(REFGUID g,PROPVARIANT*v),GetItem(g,v)) G(GetItemType,(REFGUID g,MF_ATTRIBUTE_TYPE*t),GetItemType(g,t))
    G(CompareItem,(REFGUID g,REFPROPVARIANT v,BOOL*b),CompareItem(g,v,b)) G(Compare,(IMFAttributes*a,MF_ATTRIBUTES_MATCH_TYPE t,BOOL*b),Compare(a,t,b))
    G(GetUINT32,(REFGUID g,UINT32*v),GetUINT32(g,v)) G(GetUINT64,(REFGUID g,UINT64*v),GetUINT64(g,v)) G(GetDouble,(REFGUID g,double*v),GetDouble(g,v))
    G(GetGUID,(REFGUID g,GUID*v),GetGUID(g,v)) G(GetStringLength,(REFGUID g,UINT32*v),GetStringLength(g,v))
    G(GetString,(REFGUID g,LPWSTR v,UINT32 n,UINT32*l),GetString(g,v,n,l)) G(GetAllocatedString,(REFGUID g,LPWSTR*v,UINT32*l),GetAllocatedString(g,v,l))
    G(GetBlobSize,(REFGUID g,UINT32*v),GetBlobSize(g,v)) G(GetBlob,(REFGUID g,UINT8*b,UINT32 n,UINT32*l),GetBlob(g,b,n,l))
    G(GetAllocatedBlob,(REFGUID g,UINT8**b,UINT32*n),GetAllocatedBlob(g,b,n)) G(GetUnknown,(REFGUID g,REFIID i,LPVOID*p),GetUnknown(g,i,p))
    G(SetItem,(REFGUID g,REFPROPVARIANT v),SetItem(g,v)) G(DeleteItem,(REFGUID g),DeleteItem(g)) G(DeleteAllItems,(),DeleteAllItems())
    G(SetUINT32,(REFGUID g,UINT32 v),SetUINT32(g,v)) G(SetUINT64,(REFGUID g,UINT64 v),SetUINT64(g,v)) G(SetDouble,(REFGUID g,double v),SetDouble(g,v))
    G(SetGUID,(REFGUID g,REFGUID v),SetGUID(g,v)) G(SetString,(REFGUID g,LPCWSTR v),SetString(g,v)) G(SetBlob,(REFGUID g,const UINT8*b,UINT32 n),SetBlob(g,b,n))
    G(SetUnknown,(REFGUID g,IUnknown*u),SetUnknown(g,u)) G(LockStore,(),LockStore()) G(UnlockStore,(),UnlockStore())
    G(GetCount,(UINT32*n),GetCount(n)) G(GetItemByIndex,(UINT32 n,GUID*g,PROPVARIANT*v),GetItemByIndex(n,g,v)) G(CopyAllItems,(IMFAttributes*d),CopyAllItems(d))
#undef G
};

class Factory final:public IClassFactory{
    std::atomic<ULONG>refs{1};
public:
    HRESULT QueryInterface(REFIID riid,void**ppv)override{if(!ppv)return E_POINTER;*ppv=nullptr;if(riid==IID_IUnknown||riid==IID_IClassFactory){*ppv=static_cast<IClassFactory*>(this);AddRef();return S_OK;}return E_NOINTERFACE;}
    ULONG AddRef()override{return ++refs;}ULONG Release()override{ULONG r=--refs;if(!r)delete this;return r;}
    HRESULT CreateInstance(IUnknown*outer,REFIID riid,void**ppv)override{if(outer)return CLASS_E_NOAGGREGATION;if(!ppv)return E_POINTER;*ppv=nullptr;Activator*a=new(std::nothrow)Activator();if(!a)return E_OUTOFMEMORY;HRESULT hr=a->Initialize();if(SUCCEEDED(hr))hr=a->QueryInterface(riid,ppv);a->Release();return hr;}
    HRESULT LockServer(BOOL)override{return S_OK;}
};

extern "C" __declspec(dllexport) HRESULT STDMETHODCALLTYPE DllCanUnloadNow(){return S_FALSE;}
extern "C" __declspec(dllexport) HRESULT STDMETHODCALLTYPE DllGetClassObject(REFCLSID c,REFIID r,void**p){if(c!=CLSID_4KRustCameraVirtualSource)return CLASS_E_CLASSNOTAVAILABLE;Factory*f=new(std::nothrow)Factory();if(!f)return E_OUTOFMEMORY;HRESULT h=f->QueryInterface(r,p);f->Release();return h;}
extern "C" __declspec(dllexport) HRESULT STDMETHODCALLTYPE DllRegisterServer(){
    if(!g_module)return E_UNEXPECTED;wchar_t path[MAX_PATH];DWORD n=GetModuleFileNameW(g_module,path,MAX_PATH);if(!n||n>=MAX_PATH)return HRESULT_FROM_WIN32(GetLastError());
    wchar_t clsid[64];StringFromGUID2(CLSID_4KRustCameraVirtualSource,clsid,64);std::wstring k=L"SOFTWARE\\Classes\\CLSID\\"+std::wstring(clsid)+L"\\InprocServer32";
    HKEY key=nullptr;LONG r=RegCreateKeyExW(HKEY_LOCAL_MACHINE,k.c_str(),0,nullptr,0,KEY_WRITE,nullptr,&key,nullptr);if(r!=ERROR_SUCCESS)return HRESULT_FROM_WIN32(r);
    RegSetValueExW(key,nullptr,0,REG_SZ,(BYTE*)path,(DWORD)((wcslen(path)+1)*sizeof(wchar_t)));const wchar_t model[]=L"Both";RegSetValueExW(key,L"ThreadingModel",0,REG_SZ,(BYTE*)model,sizeof(model));RegCloseKey(key);return S_OK;
}
extern "C" __declspec(dllexport) HRESULT STDMETHODCALLTYPE DllUnregisterServer(){wchar_t c[64];StringFromGUID2(CLSID_4KRustCameraVirtualSource,c,64);std::wstring k=L"SOFTWARE\\Classes\\CLSID\\"+std::wstring(c);return HRESULT_FROM_WIN32(RegDeleteTreeW(HKEY_LOCAL_MACHINE,k.c_str()));}
extern "C" __declspec(dllexport) HRESULT STDMETHODCALLTYPE Register4KRustVirtualCamera(){
    HRESULT hr=MFStartup(MF_VERSION);if(FAILED(hr))return hr;IMFVirtualCamera*vc=nullptr;
    hr=MFCreateVirtualCamera(MFVirtualCameraType_SoftwareCameraSource,MFVirtualCameraLifetime_Session,MFVirtualCameraAccess_CurrentUser,L"4K Rust Camera",VC_CLSID_STRING,nullptr,0,&vc);
    if(SUCCEEDED(hr))hr=vc->Start(nullptr);if(vc)vc->Release();MFShutdown();return hr;
}
extern "C" __declspec(dllexport) HRESULT STDMETHODCALLTYPE Remove4KRustVirtualCamera(){
    HRESULT hr=MFStartup(MF_VERSION);if(FAILED(hr))return hr;IMFVirtualCamera*vc=nullptr;
    hr=MFCreateVirtualCamera(MFVirtualCameraType_SoftwareCameraSource,MFVirtualCameraLifetime_Session,MFVirtualCameraAccess_CurrentUser,L"4K Rust Camera",VC_CLSID_STRING,nullptr,0,&vc);
    if(SUCCEEDED(hr))hr=vc->Remove();if(vc)vc->Release();MFShutdown();return hr;
}
BOOL APIENTRY DllMain(HMODULE h,DWORD reason,LPVOID){if(reason==DLL_PROCESS_ATTACH){g_module=h;DisableThreadLibraryCalls(h);}return TRUE;}

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mferror.h>
#include <string>
#include <strsafe.h>
#include <ks.h>
#include <ksmedia.h>
#include <atomic>
#include <new>
#include "FrameRing.h"

#pragma comment(lib, "mfplat.lib")
#pragma comment(lib, "mfuuid.lib")
#pragma comment(lib, "ole32.lib")
#pragma comment(lib, "advapi32.lib")

// Synthetic Media Foundation virtual-camera source.
// Frames are supplied by the Rust process through FrameRing shared memory.
// The source exposes one RGB32 video stream and produces a sample per RequestSample.

static const CLSID CLSID_4KRustCameraVirtualSource =
{0x5a1e1c48,0x5e61,0x4e7f,{0x9e,0x4a,0x1a,0x3e,0x76,0x12,0x0d,0x42}};

static const UINT32 VC_FPS_NUM = 30;
static const UINT32 VC_FPS_DEN = 1;
static HMODULE g_module = nullptr;

template<class T> static void SafeRelease(T** p) { if (p && *p) { (*p)->Release(); *p=nullptr; } }

class FrameReader {
    HANDLE mapping_ = nullptr;
    HANDLE mutex_ = nullptr;
    FrameRing* ring_ = nullptr;
public:
    ~FrameReader(){ close(); }
    HRESULT open() {
        mapping_ = OpenFileMappingW(FILE_MAP_READ, FALSE, VC_MAPPING_NAME);
        if (!mapping_) return HRESULT_FROM_WIN32(GetLastError());
        ring_ = static_cast<FrameRing*>(MapViewOfFile(mapping_, FILE_MAP_READ, 0, 0, sizeof(FrameRing)));
        if (!ring_) { HRESULT h=HRESULT_FROM_WIN32(GetLastError()); close(); return h; }
        mutex_ = OpenMutexW(SYNCHRONIZE, FALSE, VC_MUTEX_NAME);
        return S_OK; // mutex is an optimization; sequence protocol is authoritative
    }
    void close() {
        if (ring_) UnmapViewOfFile(ring_);
        ring_=nullptr;
        if (mapping_) CloseHandle(mapping_);
        mapping_=nullptr;
        if (mutex_) CloseHandle(mutex_);
        mutex_=nullptr;
    }
    bool valid() const { return ring_ != nullptr; }
    HRESULT copy(BYTE* dst, LONG pitch, UINT32 width, UINT32 height) {
        if (!ring_) return MF_E_NOT_INITIALIZED;
        if (width != VC_WIDTH || height != VC_HEIGHT) return MF_E_INVALIDMEDIATYPE;
        for (int attempt=0; attempt<4; ++attempt) {
            const uint64_t s1 = static_cast<uint64_t>(InterlockedCompareExchange64(reinterpret_cast<volatile LONG64*>(&ring_->header.sequence), 0, 0));
            if (s1 == 0 || (s1 & 1)) { Sleep(0); continue; }
            const uint32_t srcStride = ring_->header.stride;
            const uint32_t srcHeight = ring_->header.height;
            if (ring_->header.width != width || srcHeight != height || srcStride < width*4) return MF_E_INVALIDMEDIATYPE;
            for (UINT32 y=0; y<height; ++y) {
                CopyMemory(dst + y*pitch, ring_->pixels + y*srcStride, width*4);
            }
            const uint64_t s2 = static_cast<uint64_t>(InterlockedCompareExchange64(reinterpret_cast<volatile LONG64*>(&ring_->header.sequence), 0, 0));
            if (s1 == s2 && !(s2 & 1)) return S_OK;
        }
        return MF_E_NOTACCEPTING;
    }
};

class MediaSource;

class MediaStream final : public IMFMediaStream {
    std::atomic<ULONG> refs_{1};
    MediaSource* source_;
    IMFMediaEventQueue* events_=nullptr;
    IMFStreamDescriptor* descriptor_=nullptr;
    IMFMediaType* type_=nullptr;
    FrameReader reader_;
    bool running_=false;
    friend class MediaSource;
public:
    MediaStream(MediaSource* s):source_(s){}
    ~MediaStream(){ Shutdown(); SafeRelease(&events_); SafeRelease(&descriptor_); SafeRelease(&type_); }
    HRESULT Initialize();
    HRESULT RequestSample(IUnknown* token);
    HRESULT Start(IMFMediaType* type);
    HRESULT Stop();
    HRESULT Shutdown(){ running_=false; reader_.close(); return S_OK; }
    HRESULT QueryInterface(REFIID riid, void** ppv) override;
    ULONG AddRef() override { return ++refs_; }
    ULONG Release() override { ULONG n=--refs_; if(!n) delete this; return n; }
    HRESULT BeginGetEvent(IMFAsyncCallback* c,IUnknown* s) override { return events_->BeginGetEvent(c,s); }
    HRESULT EndGetEvent(IMFAsyncResult* r,IMFMediaEvent** e) override { return events_->EndGetEvent(r,e); }
    HRESULT GetEvent(DWORD f,IMFMediaEvent** e) override { return events_->GetEvent(f,e); }
    HRESULT QueueEvent(MediaEventType t,REFGUID g,HRESULT h,const PROPVARIANT* v) override { return events_->QueueEventParamVar(t,g,h,v); }
    HRESULT GetMediaSource(IMFMediaSource** s) override;
    HRESULT GetStreamDescriptor(IMFStreamDescriptor** d) override { if(!d)return E_POINTER; *d=descriptor_; return descriptor_?descriptor_->AddRef(),S_OK:E_UNEXPECTED; }
};

class MediaSource final : public IMFMediaSource {
    std::atomic<ULONG> refs_{1};
    IMFMediaEventQueue* events_=nullptr;
    IMFPresentationDescriptor* presentation_=nullptr;
    MediaStream* stream_=nullptr;
    bool started_=false;
public:
    ~MediaSource(){ Shutdown(); SafeRelease(&events_); SafeRelease(&presentation_); if(stream_) stream_->Release(); }
    HRESULT Initialize();
    HRESULT QueryInterface(REFIID riid,void** ppv) override;
    ULONG AddRef() override{return ++refs_;}
    ULONG Release() override{ULONG n=--refs_;if(!n)delete this;return n;}
    HRESULT BeginGetEvent(IMFAsyncCallback*c,IUnknown*s)override{return events_->BeginGetEvent(c,s);}
    HRESULT EndGetEvent(IMFAsyncResult*r,IMFMediaEvent**e)override{return events_->EndGetEvent(r,e);}
    HRESULT GetEvent(DWORD f,IMFMediaEvent**e)override{return events_->GetEvent(f,e);}
    HRESULT QueueEvent(MediaEventType t,REFGUID g,HRESULT h,const PROPVARIANT*v)override{return events_->QueueEventParamVar(t,g,h,v);}
    HRESULT CreatePresentationDescriptor(IMFPresentationDescriptor** pd)override{if(!pd)return E_POINTER;*pd=presentation_;return presentation_?presentation_->AddRef(),S_OK:E_UNEXPECTED;}
    HRESULT GetCharacteristics(DWORD* c)override{if(!c)return E_POINTER;*c=MFMEDIASOURCE_IS_LIVE|MFMEDIASOURCE_CAN_PAUSE;return S_OK;}
    HRESULT Start(IMFPresentationDescriptor* pd,const GUID*,const PROPVARIANT*)override;
    HRESULT Stop()override;
    HRESULT Pause()override{return S_OK;}
    HRESULT Shutdown()override{if(stream_)stream_->Shutdown();started_=false;return S_OK;}
    MediaStream* stream(){return stream_;}
};

HRESULT MediaStream::QueryInterface(REFIID riid,void** ppv){if(!ppv)return E_POINTER;*ppv=nullptr;if(riid==IID_IUnknown||riid==IID_IMFMediaStream){*ppv=static_cast<IMFMediaStream*>(this);AddRef();return S_OK;}return E_NOINTERFACE;}
HRESULT MediaStream::GetMediaSource(IMFMediaSource** s){if(!s)return E_POINTER;*s=reinterpret_cast<IMFMediaSource*>(source_);source_->AddRef();return S_OK;}
HRESULT MediaStream::Initialize(){
    HRESULT hr=MFCreateEventQueue(&events_); if(FAILED(hr))return hr;
    hr=MFCreateMediaType(&type_); if(FAILED(hr))return hr;
    type_->SetGUID(MF_MT_MAJOR_TYPE,MFMediaType_Video);
    type_->SetGUID(MF_MT_SUBTYPE,MFVideoFormat_RGB32);
    type_->SetUINT32(MF_MT_INTERLACE_MODE,MFVideoInterlace_Progressive);
    type_->SetUINT32(MF_MT_ALL_SAMPLES_INDEPENDENT,TRUE);
    MFSetAttributeSize(type_,MF_MT_FRAME_SIZE,VC_WIDTH,VC_HEIGHT);
    MFSetAttributeRatio(type_,MF_MT_FRAME_RATE,VC_FPS_NUM,VC_FPS_DEN);
    MFSetAttributeRatio(type_,MF_MT_PIXEL_ASPECT_RATIO,1,1);
    type_->SetUINT32(MF_MT_DEFAULT_STRIDE,VC_STRIDE);
    IMFMediaType* list[1]={type_}; hr=MFCreateStreamDescriptor(0,1,list,&descriptor_); if(FAILED(hr))return hr;
    IMFMediaTypeHandler* h=nullptr; hr=descriptor_->GetMediaTypeHandler(&h); if(SUCCEEDED(hr))hr=h->SetCurrentMediaType(type_); SafeRelease(&h);
    return hr;
}
HRESULT MediaStream::Start(IMFMediaType* type){if(type){BOOL same=FALSE; type_->Compare(type,MF_ATTRIBUTES_MATCH_ALL_ITEMS,&same);if(!same)return MF_E_INVALIDMEDIATYPE;}if(!reader_.valid()){HRESULT hr=reader_.open();if(FAILED(hr))return hr;}running_=true;return events_->QueueEventParamVar(MEStreamStarted,GUID_NULL,S_OK,nullptr);}
HRESULT MediaStream::Stop(){running_=false;return events_->QueueEventParamVar(MEStreamStopped,GUID_NULL,S_OK,nullptr);}
HRESULT MediaStream::RequestSample(IUnknown* token){
    if(!running_)return MF_E_INVALIDREQUEST;
    IMFMediaBuffer* b=nullptr; IMFSample* sample=nullptr;
    HRESULT hr=MFCreateMemoryBuffer(VC_BYTES,&b); if(FAILED(hr))return hr;
    BYTE* p=nullptr; DWORD max=0,cur=0; hr=b->Lock(&p,&max,&cur);
    if(SUCCEEDED(hr)){ hr=reader_.copy(p,VC_STRIDE,VC_WIDTH,VC_HEIGHT); b->Unlock(); }
    if(SUCCEEDED(hr)) hr=MFCreateSample(&sample);
    if(SUCCEEDED(hr)) hr=sample->AddBuffer(b);
    if(SUCCEEDED(hr)) hr=sample->SetSampleTime(MFGetSystemTime());
    if(SUCCEEDED(hr)) hr=sample->SetSampleDuration(10000000/VC_FPS_NUM);
    if(SUCCEEDED(hr)&&token) hr=sample->SetUnknown(MFSampleExtension_Token,token);
    if(SUCCEEDED(hr)) hr=events_->QueueEventParamUnk(MEMediaSample,GUID_NULL,S_OK,sample);
    SafeRelease(&sample);SafeRelease(&b);return hr;
}

HRESULT MediaSource::QueryInterface(REFIID riid,void** ppv){if(!ppv)return E_POINTER;*ppv=nullptr;if(riid==IID_IUnknown||riid==IID_IMFMediaSource){*ppv=static_cast<IMFMediaSource*>(this);AddRef();return S_OK;}return E_NOINTERFACE;}
HRESULT MediaSource::Initialize(){HRESULT hr=MFCreateEventQueue(&events_);if(FAILED(hr))return hr;stream_=new(std::nothrow) MediaStream(this);if(!stream_)return E_OUTOFMEMORY;hr=stream_->Initialize();if(FAILED(hr))return hr;IMFStreamDescriptor* sd=nullptr;hr=stream_->GetStreamDescriptor(&sd);if(FAILED(hr))return hr;hr=MFCreatePresentationDescriptor(1,&sd,&presentation_);sd->Release();return hr;}
HRESULT MediaSource::Start(IMFPresentationDescriptor* pd,const GUID*,const PROPVARIANT*){if(!pd)return E_POINTER;BOOL selected=FALSE;IMFStreamDescriptor* sd=nullptr;HRESULT hr=pd->GetStreamDescriptorByIndex(0,&selected,&sd);SafeRelease(&sd);if(FAILED(hr)||!selected)return MF_E_INVALIDREQUEST;hr=stream_->Start(stream_->type_);if(FAILED(hr))return hr;started_=true;events_->QueueEventParamVar(MESourceStarted,GUID_NULL,S_OK,nullptr);return hr;}
HRESULT MediaSource::Stop(){started_=false;stream_->Stop();return events_->QueueEventParamVar(MESourceStopped,GUID_NULL,S_OK,nullptr);}

class Activate final : public IMFActivate {
    std::atomic<ULONG> refs_{1}; IMFAttributes* attrs_=nullptr; MediaSource* source_=nullptr;
public:
    ~Activate(){SafeRelease(&attrs_);if(source_)source_->Release();}
    HRESULT Initialize(){return MFCreateAttributes(&attrs_,4);}
    HRESULT QueryInterface(REFIID riid,void** ppv)override{if(!ppv)return E_POINTER;*ppv=nullptr;if(riid==IID_IUnknown||riid==IID_IMFActivate||riid==IID_IMFAttributes){*ppv=static_cast<IMFActivate*>(this);AddRef();return S_OK;}return E_NOINTERFACE;}
    ULONG AddRef()override{return ++refs_;}
    ULONG Release()override{ULONG n=--refs_;if(!n)delete this;return n;}
    HRESULT ActivateObject(REFIID riid,void** ppv)override{if(!ppv)return E_POINTER;*ppv=nullptr;if(!source_){source_=new(std::nothrow) MediaSource();if(!source_)return E_OUTOFMEMORY;HRESULT hr=source_->Initialize();if(FAILED(hr)){source_->Release();source_=nullptr;return hr;}}return source_->QueryInterface(riid,ppv);}
    HRESULT ShutdownObject()override{return S_OK;} HRESULT DetachObject()override{if(source_){source_->Shutdown();source_->Release();source_=nullptr;}return S_OK;}
#define ATTR(m) HRESULT m(REFGUID k, ##__VA_ARGS__) override { return attrs_->m(k, ##__VA_ARGS__); }
    HRESULT GetItem(REFGUID k,PROPVARIANT* v)override{return attrs_->GetItem(k,v);}
    HRESULT GetItemType(REFGUID k,MF_ATTRIBUTE_TYPE* t)override{return attrs_->GetItemType(k,t);}
    HRESULT CompareItem(REFGUID k,REFPROPVARIANT v,BOOL*r)override{return attrs_->CompareItem(k,v,r);}
    HRESULT Compare(IMFAttributes*a,MF_ATTRIBUTES_MATCH_TYPE t,BOOL*r)override{return attrs_->Compare(a,t,r);}
    HRESULT GetUINT32(REFGUID k,UINT32*v)override{return attrs_->GetUINT32(k,v);} HRESULT GetUINT64(REFGUID k,UINT64*v)override{return attrs_->GetUINT64(k,v);}
    HRESULT GetDouble(REFGUID k,double*v)override{return attrs_->GetDouble(k,v);} HRESULT GetGUID(REFGUID k,GUID*v)override{return attrs_->GetGUID(k,v);}
    HRESULT GetStringLength(REFGUID k,UINT32*v)override{return attrs_->GetStringLength(k,v);} HRESULT GetString(REFGUID k,LPWSTR v,UINT32 n,UINT32*l)override{return attrs_->GetString(k,v,n,l);}
    HRESULT GetAllocatedString(REFGUID k,LPWSTR*v,UINT32*l)override{return attrs_->GetAllocatedString(k,v,l);} HRESULT GetBlobSize(REFGUID k,UINT32*v)override{return attrs_->GetBlobSize(k,v);}
    HRESULT GetBlob(REFGUID k,UINT8*v,UINT32 n,UINT32*l)override{return attrs_->GetBlob(k,v,n,l);} HRESULT GetAllocatedBlob(REFGUID k,UINT8**v,UINT32*l)override{return attrs_->GetAllocatedBlob(k,v,l);}
    HRESULT GetUnknown(REFGUID k,REFIID i,LPVOID*v)override{return attrs_->GetUnknown(k,i,v);}
    HRESULT SetItem(REFGUID k,REFPROPVARIANT v)override{return attrs_->SetItem(k,v);} HRESULT DeleteItem(REFGUID k)override{return attrs_->DeleteItem(k);}
    HRESULT DeleteAllItems()override{return attrs_->DeleteAllItems();} HRESULT SetUINT32(REFGUID k,UINT32 v)override{return attrs_->SetUINT32(k,v);}
    HRESULT SetUINT64(REFGUID k,UINT64 v)override{return attrs_->SetUINT64(k,v);} HRESULT SetDouble(REFGUID k,double v)override{return attrs_->SetDouble(k,v);}
    HRESULT SetGUID(REFGUID k,REFGUID v)override{return attrs_->SetGUID(k,v);} HRESULT SetString(REFGUID k,LPCWSTR v)override{return attrs_->SetString(k,v);}
    HRESULT SetBlob(REFGUID k,const UINT8*v,UINT32 n)override{return attrs_->SetBlob(k,v,n);} HRESULT SetUnknown(REFGUID k,IUnknown*v)override{return attrs_->SetUnknown(k,v);}
    HRESULT LockStore()override{return attrs_->LockStore();} HRESULT UnlockStore()override{return attrs_->UnlockStore();} HRESULT GetCount(UINT32*v)override{return attrs_->GetCount(v);}
    HRESULT GetItemByIndex(UINT32 i,GUID*k,PROPVARIANT*v)override{return attrs_->GetItemByIndex(i,k,v);} HRESULT CopyAllItems(IMFAttributes*d)override{return attrs_->CopyAllItems(d);}
};

class Factory final : public IClassFactory {
    std::atomic<ULONG> refs_{1};
public:
    HRESULT QueryInterface(REFIID riid,void**ppv)override{if(!ppv)return E_POINTER;*ppv=nullptr;if(riid==IID_IUnknown||riid==IID_IClassFactory){*ppv=this;AddRef();return S_OK;}return E_NOINTERFACE;}
    ULONG AddRef()override{return ++refs_;} ULONG Release()override{ULONG n=--refs_;if(!n)delete this;return n;}
    HRESULT CreateInstance(IUnknown*,REFIID riid,void**ppv)override{if(!ppv)return E_POINTER;*ppv=nullptr;Activate*a=new(std::nothrow)Activate();if(!a)return E_OUTOFMEMORY;HRESULT hr=a->Initialize();if(SUCCEEDED(hr))hr=a->QueryInterface(riid,ppv);a->Release();return hr;}
    HRESULT LockServer(BOOL)override{return S_OK;}
};

extern "C" HRESULT STDAPICALLTYPE DllGetClassObject(REFCLSID clsid,REFIID riid,void**ppv){
    if(clsid!=CLSID_4KRustCameraVirtualSource)return CLASS_E_CLASSNOTAVAILABLE;
    Factory*f=new(std::nothrow)Factory();if(!f)return E_OUTOFMEMORY;HRESULT hr=f->QueryInterface(riid,ppv);f->Release();return hr;
}
extern "C" HRESULT STDAPICALLTYPE DllCanUnloadNow(){return S_FALSE;}
BOOL APIENTRY DllMain(HMODULE hModule,DWORD reason,LPVOID){ if(reason==DLL_PROCESS_ATTACH){g_module=hModule; DisableThreadLibraryCalls(hModule);} return TRUE; }

extern "C" __declspec(dllexport) HRESULT STDMETHODCALLTYPE DllRegisterServer() {
    wchar_t module[MAX_PATH]{}; if(!GetModuleFileNameW(g_module,module,MAX_PATH)) return HRESULT_FROM_WIN32(GetLastError());
    wchar_t clsid[64]{}; StringFromGUID2(CLSID_4KRustCameraVirtualSource,clsid,64);
    std::wstring key=L"Software\\Classes\\CLSID\\"+std::wstring(clsid)+L"\\InprocServer32"; HKEY h=nullptr;
    LONG rc=RegCreateKeyExW(HKEY_CURRENT_USER,key.c_str(),0,nullptr,0,KEY_SET_VALUE,nullptr,&h,nullptr); if(rc!=ERROR_SUCCESS)return HRESULT_FROM_WIN32(rc);
    rc=RegSetValueExW(h,nullptr,0,REG_SZ,reinterpret_cast<const BYTE*>(module),(DWORD)((wcslen(module)+1)*sizeof(wchar_t)));
    if(rc==ERROR_SUCCESS){const wchar_t* tm=L"Both";rc=RegSetValueExW(h,L"ThreadingModel",0,REG_SZ,reinterpret_cast<const BYTE*>(tm),(DWORD)((wcslen(tm)+1)*sizeof(wchar_t)));}
    RegCloseKey(h); return HRESULT_FROM_WIN32(rc);
}
extern "C" __declspec(dllexport) HRESULT STDMETHODCALLTYPE DllUnregisterServer() {
    wchar_t clsid[64]{}; StringFromGUID2(CLSID_4KRustCameraVirtualSource,clsid,64);
    std::wstring key=L"Software\\Classes\\CLSID\\"+std::wstring(clsid); LONG rc=RegDeleteTreeW(HKEY_CURRENT_USER,key.c_str());
    return rc==ERROR_FILE_NOT_FOUND?S_OK:HRESULT_FROM_WIN32(rc);
}

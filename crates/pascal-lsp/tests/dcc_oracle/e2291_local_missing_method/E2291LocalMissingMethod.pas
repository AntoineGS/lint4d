unit E2291LocalMissingMethod;
interface
type
  TObject = class
  end;
  IRequired = interface
    procedure Required;
  end;
  TImpl = class(TObject, IRequired)
    function QueryInterface(const IID: TGUID; out Obj): HResult; stdcall;
    function _AddRef: Integer; stdcall;
    function _Release: Integer; stdcall;
  end;
implementation
function TImpl.QueryInterface(const IID: TGUID; out Obj): HResult; stdcall;
begin
  Result := 0;
end;
function TImpl._AddRef: Integer; stdcall;
begin
  Result := 0;
end;
function TImpl._Release: Integer; stdcall;
begin
  Result := 0;
end;
end.

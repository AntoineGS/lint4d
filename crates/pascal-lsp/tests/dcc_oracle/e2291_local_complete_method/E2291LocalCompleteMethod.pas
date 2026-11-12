unit E2291LocalCompleteMethod;
interface
type
  TObject = class
  end;
  IRequired = interface
    procedure Required;
  end;
  TImpl = class(TObject, IRequired)
    procedure Required;
    function QueryInterface(const IID: TGUID; out Obj): HResult; stdcall;
    function _AddRef: Integer; stdcall;
    function _Release: Integer; stdcall;
  end;
implementation
procedure TImpl.Required;
begin
end;
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

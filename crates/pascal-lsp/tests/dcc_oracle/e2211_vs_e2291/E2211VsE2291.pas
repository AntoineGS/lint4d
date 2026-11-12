unit E2211VsE2291;

interface

type
  TObject = class
  end;

  IRequired = interface
    procedure Required(Value: Integer);
  end;

  TWrongSignature = class(TObject, IRequired)
    procedure Required(Value: string);
    function QueryInterface(const IID: TGUID; out Obj): HResult; stdcall;
    function _AddRef: Integer; stdcall;
    function _Release: Integer; stdcall;
  end;

  TMissing = class(TObject, IRequired)
    function QueryInterface(const IID: TGUID; out Obj): HResult; stdcall;
    function _AddRef: Integer; stdcall;
    function _Release: Integer; stdcall;
  end;

implementation

procedure TWrongSignature.Required(Value: string);
begin
end;

function TWrongSignature.QueryInterface(const IID: TGUID; out Obj): HResult; stdcall;
begin
  Result := 0;
end;

function TWrongSignature._AddRef: Integer; stdcall;
begin
  Result := 0;
end;

function TWrongSignature._Release: Integer; stdcall;
begin
  Result := 0;
end;

function TMissing.QueryInterface(const IID: TGUID; out Obj): HResult; stdcall;
begin
  Result := 0;
end;

function TMissing._AddRef: Integer; stdcall;
begin
  Result := 0;
end;

function TMissing._Release: Integer; stdcall;
begin
  Result := 0;
end;

end.

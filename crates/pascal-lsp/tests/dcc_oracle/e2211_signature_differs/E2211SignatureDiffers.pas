unit E2211SignatureDiffers;

interface

type
  TObject = class
  end;

  IRequired = interface
    procedure Required(Value: Integer);
  end;

  TImplementation = class(TObject, IRequired)
    procedure Required(Value: string);
    function QueryInterface(const IID: TGUID; out Obj): HResult; stdcall;
    function _AddRef: Integer; stdcall;
    function _Release: Integer; stdcall;
  end;

implementation

procedure TImplementation.Required(Value: string);
begin
end;

function TImplementation.QueryInterface(const IID: TGUID; out Obj): HResult; stdcall;
begin
  Result := 0;
end;

function TImplementation._AddRef: Integer; stdcall;
begin
  Result := 0;
end;

function TImplementation._Release: Integer; stdcall;
begin
  Result := 0;
end;

end.

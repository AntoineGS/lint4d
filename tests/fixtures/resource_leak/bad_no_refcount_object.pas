unit bad_no_refcount_object;

interface

uses
  System.SysUtils;

type
  TMyNoRefObj = class(TNoRefCountObject)
    procedure DoWork;
  end;

implementation

procedure TMyNoRefObj.DoWork;
begin
end;

procedure TestDirectNoRefCount;
var
  aItf: IInterface;
begin
  // TNoRefCountObject._AddRef/_Release return -1 (no ref counting).
  // This WILL leak despite being assigned to an interface variable.
  aItf := TNoRefCountObject.Create;

  if aItf = nil then
    raise Exception.Create('should not happen');
end;

end.

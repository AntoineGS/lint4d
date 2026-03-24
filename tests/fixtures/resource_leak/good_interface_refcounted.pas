unit good_interface_refcounted;

interface

uses
  MyServiceIntf;

type
  IMyService = interface
    procedure DoWork;
  end;

  TMyService = class(TInterfacedObject, IMyService)
    procedure DoWork;
  end;

implementation

procedure TMyService.DoWork;
begin
end;

procedure TestDirectInterfaceAssign;
var
  aService: IMyService;
begin
  aService := TMyService.Create;

  if aService = nil then
    raise Exception.Create('should not happen');
end;

procedure TestIndirectInterfaceAssign;
var
  aObj: TMyService;
  aItf: IMyService;
begin
  aObj := TMyService.Create;
  aItf := aObj;

  if aObj.RefCount = 1 then
    raise Exception.Create('test');
end;

end.

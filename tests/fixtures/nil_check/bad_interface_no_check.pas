unit bad_interface_no_check;
interface
implementation
uses System, MyIntf;
procedure Test(AIntf: IMyInterface);
begin
  AIntf.DoWork;
end;
end.

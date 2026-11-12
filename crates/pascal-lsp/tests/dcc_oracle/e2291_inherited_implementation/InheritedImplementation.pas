unit InheritedImplementation;
interface
type
  TObject = class
  end;
  IRequired = interface(IInterface)
    procedure Required;
  end;
  TBase = class(TObject)
  public
    procedure Required;
  end;
  TImplementation = class(TBase, IRequired)
  end;
implementation
procedure TBase.Required;
begin
end;
end.

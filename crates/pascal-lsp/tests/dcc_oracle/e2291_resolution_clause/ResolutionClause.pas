unit ResolutionClause;
interface
type
  TObject = class
  end;
  IRequired = interface(IInterface)
    procedure Required;
  end;
  TImplementation = class(TObject, IRequired)
    procedure Implement;
    procedure IRequired.Required = Implement;
  end;
implementation
procedure TImplementation.Implement;
begin
end;
end.

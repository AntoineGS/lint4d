unit InterfacePrefixFix;

interface

type
  Printable = interface
    procedure Print;
  end;

  TDoc = class(TObject, Printable)
  public
    procedure Print;
  end;

implementation

procedure TDoc.Print;
begin
end;

end.
